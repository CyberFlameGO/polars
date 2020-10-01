use crate::lazy::prelude::*;
use crate::lazy::utils::{expr_to_root_column, rename_expr_root_name};
use crate::prelude::*;
use fnv::FnvHashMap;
use std::rc::Rc;

// check if a selection/projection can be done on the downwards schema
fn check_down_node(expr: &Expr, down_schema: &Schema) -> bool {
    expr.to_field(down_schema).is_ok()
}

pub trait Optimize {
    fn optimize(&self, logical_plan: LogicalPlan) -> LogicalPlan;
}

pub struct ProjectionPushDown {}

impl ProjectionPushDown {
    fn finish_at_leaf(&self, lp: LogicalPlan, acc_projections: Vec<Expr>) -> LogicalPlan {
        match acc_projections.len() {
            // There was no Projection in the logical plan
            0 => lp,
            _ => LogicalPlanBuilder::from(lp)
                .project(acc_projections)
                .build(),
        }
    }

    // split in a projection vec that can be pushed down and a projection vec that should be used
    // in this node
    fn split_acc_projections(
        &self,
        acc_projections: Vec<Expr>,
        down_schema: &Schema,
    ) -> (Vec<Expr>, Vec<Expr>) {
        // If node above has as many columns as the projection there is nothing to pushdown.
        if down_schema.fields().len() == acc_projections.len() {
            let local_projections = acc_projections;
            (vec![], local_projections)
        } else {
            let (acc_projections, local_projections) = acc_projections
                .into_iter()
                .partition(|expr| check_down_node(expr, down_schema));
            (acc_projections, local_projections)
        }
    }

    fn finish_node(
        &self,
        local_projections: Vec<Expr>,
        builder: LogicalPlanBuilder,
    ) -> LogicalPlan {
        if local_projections.len() > 0 {
            builder.project(local_projections).build()
        } else {
            builder.build()
        }
    }

    fn join_push_down(
        &self,
        schema_left: &Schema,
        schema_right: &Schema,
        proj: &Expr,
        pushdown_left: &mut Vec<Expr>,
        pushdown_right: &mut Vec<Expr>,
    ) -> bool {
        let mut pushed_at_least_one = false;

        if check_down_node(&proj, schema_left) {
            pushdown_left.push(proj.clone());
            pushed_at_least_one = true;
        }
        if check_down_node(&proj, schema_right) {
            pushdown_right.push(proj.clone());
            pushed_at_least_one = true;
        }
        pushed_at_least_one
    }

    // We recurrently traverse the logical plan and every projection we encounter we add to the accumulated
    // projections.
    // Every non projection operation we recurse and rebuild that operation on the output of the recursion.
    // The recursion stops at the nodes of the logical plan. These nodes IO or existing DataFrames. On top of
    // these nodes we apply the projection.
    // TODO: renaming operations and joins interfere with the schema. We need to keep track of the schema somehow.
    fn push_down(&self, logical_plan: LogicalPlan, mut acc_projections: Vec<Expr>) -> LogicalPlan {
        use LogicalPlan::*;
        match logical_plan {
            Projection { expr, input, .. } => {
                for e in expr {
                    acc_projections.push(e);
                }

                let (acc_projections, local_projections) =
                    self.split_acc_projections(acc_projections, input.schema());

                let lp = self.push_down(*input, acc_projections);
                let builder = LogicalPlanBuilder::from(lp);
                self.finish_node(local_projections, builder)
            }
            DataFrameScan { df, schema } => {
                let lp = DataFrameScan { df, schema };
                self.finish_at_leaf(lp, acc_projections)
            }
            CsvScan {
                path,
                schema,
                has_header,
                delimiter,
            } => {
                let lp = CsvScan {
                    path,
                    schema,
                    has_header,
                    delimiter,
                };
                self.finish_at_leaf(lp, acc_projections)
            }
            Sort {
                input,
                column,
                reverse,
            } => LogicalPlanBuilder::from(self.push_down(*input, acc_projections))
                .sort(column, reverse)
                .build(),
            Selection { predicate, input } => {
                LogicalPlanBuilder::from(self.push_down(*input, acc_projections))
                    .filter(predicate)
                    .build()
            }
            Aggregate {
                input, keys, aggs, ..
            } => {
                // TODO: projections of resulting columns of gb, should be renamed and pushed down
                let (acc_projections, local_projections) =
                    self.split_acc_projections(acc_projections, input.schema());

                let lp = self.push_down(*input, acc_projections);
                let builder = LogicalPlanBuilder::from(lp).groupby(keys, aggs);
                self.finish_node(local_projections, builder)
            }
            Join {
                input_left,
                input_right,
                left_on,
                right_on,
                how,
                ..
            } => {
                let mut pushdown_left = vec![];
                let mut pushdown_right = vec![];
                let mut local_projection = vec![];

                // if there are no projections we don't have to do anything
                if acc_projections.len() > 0 {
                    let schema_left = input_left.schema();
                    let schema_right = input_right.schema();

                    // We need the join columns so we push the projection downwards
                    pushdown_left.push(Expr::Column(left_on.clone()));
                    pushdown_right.push(Expr::Column(right_on.clone()));

                    for mut proj in acc_projections {
                        let mut add_local = true;

                        // if it is an alias we want to project the root column name downwards
                        // but we don't want to project it a this level, otherwise we project both
                        // the root and the alias, hence add_local = false.
                        if let Expr::Alias(expr, name) = proj {
                            let root_name = expr_to_root_column(&expr).unwrap();

                            proj = Expr::Column(root_name);
                            local_projection.push(Expr::Alias(Box::new(proj.clone()), name));

                            // now we don
                            add_local = false;
                        }

                        // Path for renamed columns due to the join. The column name of the left table
                        // stays as is, the column of the right will have the "_right" suffix.
                        // Thus joining two tables with both a foo column leads to ["foo", "foo_right"]
                        if !self.join_push_down(
                            schema_left,
                            schema_right,
                            &proj,
                            &mut pushdown_left,
                            &mut pushdown_right,
                        ) {
                            // Column name of the projection without any alias.
                            let root_column_name = expr_to_root_column(&proj).unwrap();

                            // If _right suffix exists we need to push a projection down without this
                            // suffix.
                            if root_column_name.ends_with("_right") {
                                // downwards name is the name without the _right i.e. "foo".
                                let (downwards_name, _) = root_column_name
                                    .split_at(root_column_name.len() - "_right".len());

                                // project downwards and immediately alias to prevent wrong projections
                                let projection =
                                    col(downwards_name).alias(&format!("{}_right", downwards_name));
                                pushdown_right.push(projection);
                                // locally we project the aliased column
                                local_projection.push(proj);
                            }
                        } else if add_local {
                            // always also do the projection locally, because the join columns may not be
                            // included in the projection.
                            // for instance:
                            //
                            // SELECT [COLUMN temp]
                            // FROM
                            // JOIN (["days", "temp"]) WITH (["days", "rain"]) ON (left: days right: days)
                            //
                            // should drop the days column afther the join.
                            local_projection.push(proj)
                        }
                    }
                }
                let lp_left = self.push_down(*input_left, pushdown_left);
                let lp_right = self.push_down(*input_right, pushdown_right);
                let builder =
                    LogicalPlanBuilder::from(lp_left).join(lp_right, how, left_on, right_on);
                self.finish_node(local_projection, builder)
            }
        }
    }
}

impl Optimize for ProjectionPushDown {
    fn optimize(&self, logical_plan: LogicalPlan) -> LogicalPlan {
        self.push_down(logical_plan, Vec::default())
    }
}

pub struct PredicatePushDown {}

impl PredicatePushDown {
    fn finish_at_leaf(
        &self,
        lp: LogicalPlan,
        acc_predicates: FnvHashMap<Rc<String>, Expr>,
    ) -> LogicalPlan {
        match acc_predicates.len() {
            // No filter in the logical plan
            0 => lp,
            _ => {
                // TODO: create a single predicate
                let mut builder = LogicalPlanBuilder::from(lp);
                for expr in acc_predicates.values() {
                    builder = builder.filter(expr.clone());
                }
                builder.build()
            }
        }
    }

    fn finish_node(
        &self,
        local_predicates: Vec<Expr>,
        mut builder: LogicalPlanBuilder,
    ) -> LogicalPlan {
        if local_predicates.len() > 0 {
            for expr in local_predicates {
                builder = builder.filter(expr);
            }
            builder.build()
        } else {
            builder.build()
        }
    }

    // acc predicates maps the root column names to predicates
    fn push_down(
        &self,
        logical_plan: LogicalPlan,
        mut acc_predicates: FnvHashMap<Rc<String>, Expr>,
    ) -> LogicalPlan {
        use LogicalPlan::*;

        match logical_plan {
            Selection { predicate, input } => {
                match expr_to_root_column(&predicate) {
                    Ok(name) => {
                        acc_predicates.insert(name, predicate);
                    }
                    Err(_) => panic!("implement logic for binary expr with 2 root columns"),
                }
                self.push_down(*input, acc_predicates)
            }
            Projection { expr, input, .. } => {
                // maybe update predicate name if a projection is an alias
                for e in &expr {
                    // check if there is an alias
                    if let Expr::Alias(e, name) = e {
                        // if this alias refers to one of the predicates in the upper nodes
                        // we rename the column of the predicate before we push it downwards.
                        if let Some(predicate) = acc_predicates.remove(name) {
                            let new_name = expr_to_root_column(e).unwrap();
                            let new_predicate =
                                rename_expr_root_name(&predicate, new_name.clone()).unwrap();
                            acc_predicates.insert(new_name, new_predicate);
                        }
                    }
                }
                LogicalPlanBuilder::from(self.push_down(*input, acc_predicates))
                    .project(expr)
                    .build()
            }
            DataFrameScan { df, schema } => {
                let lp = DataFrameScan { df, schema };
                self.finish_at_leaf(lp, acc_predicates)
            }
            CsvScan {
                path,
                schema,
                has_header,
                delimiter,
            } => {
                let lp = CsvScan {
                    path,
                    schema,
                    has_header,
                    delimiter,
                };
                self.finish_at_leaf(lp, acc_predicates)
            }
            Sort {
                input,
                column,
                reverse,
            } => LogicalPlanBuilder::from(self.push_down(*input, acc_predicates))
                .sort(column, reverse)
                .build(),
            Aggregate {
                input, keys, aggs, ..
            } => LogicalPlanBuilder::from(self.push_down(*input, acc_predicates))
                .groupby(keys, aggs)
                .build(),
            Join {
                input_left,
                input_right,
                left_on,
                right_on,
                how,
                ..
            } => {
                let schema_left = input_left.schema();
                let schema_right = input_right.schema();

                let mut pushdown_left = FnvHashMap::default();
                let mut pushdown_right = FnvHashMap::default();
                let mut local_predicates = vec![];

                for predicate in acc_predicates.values() {
                    if check_down_node(&predicate, schema_left) {
                        let name = Rc::new(predicate.to_field(schema_left).unwrap().name().clone());
                        pushdown_left.insert(name, predicate.clone());
                    } else if check_down_node(&predicate, schema_right) {
                        let name =
                            Rc::new(predicate.to_field(schema_right).unwrap().name().clone());
                        pushdown_right.insert(name, predicate.clone());
                    } else {
                        local_predicates.push(predicate.clone())
                    }
                }

                let lp_left = self.push_down(*input_left, pushdown_left);
                let lp_right = self.push_down(*input_right, pushdown_right);

                let builder =
                    LogicalPlanBuilder::from(lp_left).join(lp_right, how, left_on, right_on);
                self.finish_node(local_predicates, builder)
            }
        }
    }
}

impl Optimize for PredicatePushDown {
    fn optimize(&self, logical_plan: LogicalPlan) -> LogicalPlan {
        self.push_down(logical_plan, FnvHashMap::default())
    }
}

#[cfg(test)]
mod test {
    use crate::lazy::logical_plan::optimizer::Optimize;
    use crate::lazy::prelude::*;
    use crate::lazy::tests::get_df;

    #[test]
    fn test_logical_plan() {
        let df = get_df();

        // expensive order
        let lf = df
            .clone()
            .lazy()
            .sort("sepal.width", false)
            .select(&[col("sepal.width")]);

        let logical_plan = lf.logical_plan;
        let opt = ProjectionPushDown {};
        let logical_plan = opt.optimize(logical_plan);
        println!("{}", logical_plan.describe());
    }
}
