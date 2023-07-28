mod join;
mod keys;
mod rename;
mod utils;

use polars_core::datatypes::PlHashMap;
use polars_core::prelude::*;
use utils::*;

use super::*;
use crate::dsl::function_expr::FunctionExpr;
use crate::logical_plan::{optimizer, Context};
use crate::prelude::optimizer::predicate_pushdown::join::process_join;
use crate::prelude::optimizer::predicate_pushdown::rename::process_rename;
use crate::utils::{aexprs_to_schema, check_input_node, has_aexpr};

#[derive(Default)]
pub struct PredicatePushDown {}

impl PredicatePushDown {
    fn optional_apply_predicate(
        &self,
        lp: ALogicalPlan,
        local_predicates: Vec<Node>,
        lp_arena: &mut Arena<ALogicalPlan>,
        expr_arena: &mut Arena<AExpr>,
    ) -> ALogicalPlan {
        if !local_predicates.is_empty() {
            let predicate = combine_predicates(local_predicates.into_iter(), expr_arena);
            let input = lp_arena.add(lp);

            ALogicalPlan::Selection { input, predicate }
        } else {
            lp
        }
    }

    fn pushdown_and_assign(
        &self,
        input: Node,
        acc_predicates: PlHashMap<Arc<str>, Node>,
        lp_arena: &mut Arena<ALogicalPlan>,
        expr_arena: &mut Arena<AExpr>,
    ) -> PolarsResult<()> {
        let alp = lp_arena.take(input);
        let lp = self.push_down(alp, acc_predicates, lp_arena, expr_arena)?;
        lp_arena.replace(input, lp);
        Ok(())
    }

    /// Filter will be pushed down.
    fn pushdown_and_continue(
        &self,
        lp: ALogicalPlan,
        mut acc_predicates: PlHashMap<Arc<str>, Node>,
        lp_arena: &mut Arena<ALogicalPlan>,
        expr_arena: &mut Arena<AExpr>,
        has_projections: bool,
    ) -> PolarsResult<ALogicalPlan> {
        let inputs = lp.get_inputs();
        let exprs = lp.get_exprs();

        if has_projections {
            // we should not pass these projections
            if exprs
                .iter()
                .any(|e_n| projection_is_definite_pushdown_boundary(*e_n, expr_arena))
            {
                return self.no_pushdown_restart_opt(lp, acc_predicates, lp_arena, expr_arena);
            }

            // projections should only have a single input.
            if inputs.len() > 1 {
                // except for ExtContext
                assert!(matches!(lp, ALogicalPlan::ExtContext { .. }));
            }
            let input = inputs[inputs.len() - 1];
            let (local_predicates, projections) =
                rewrite_projection_node(expr_arena, lp_arena, &mut acc_predicates, exprs, input);

            let alp = lp_arena.take(input);
            let alp = self.push_down(alp, acc_predicates, lp_arena, expr_arena)?;
            lp_arena.replace(input, alp);

            let lp = lp.with_exprs_and_input(projections, inputs);
            Ok(self.optional_apply_predicate(lp, local_predicates, lp_arena, expr_arena))
        } else {
            let mut local_predicates = Vec::with_capacity(acc_predicates.len());

            // determine new inputs by pushing down predicates
            let new_inputs = inputs
                .iter()
                .map(|&node| {
                    // first we check if we are able to push down the predicate passed this node
                    // it could be that this node just added the column where we base the predicate on
                    let input_schema = lp_arena.get(node).schema(lp_arena);
                    let mut pushdown_predicates =
                        optimizer::init_hashmap(Some(acc_predicates.len()));
                    for (_, &predicate) in acc_predicates.iter() {
                        // we can pushdown the predicate
                        if check_input_node(predicate, &input_schema, expr_arena) {
                            insert_and_combine_predicate(
                                &mut pushdown_predicates,
                                predicate,
                                expr_arena,
                            )
                        }
                        // we cannot pushdown the predicate we do it here
                        else {
                            local_predicates.push(predicate);
                        }
                    }

                    let alp = lp_arena.take(node);
                    let alp = self.push_down(alp, pushdown_predicates, lp_arena, expr_arena)?;
                    lp_arena.replace(node, alp);
                    Ok(node)
                })
                .collect::<PolarsResult<Vec<_>>>()?;

            let lp = lp.with_exprs_and_input(exprs, new_inputs);
            Ok(self.optional_apply_predicate(lp, local_predicates, lp_arena, expr_arena))
        }
    }

    /// Filter will be done at this node, but we continue optimization
    fn no_pushdown_restart_opt(
        &self,
        lp: ALogicalPlan,
        acc_predicates: PlHashMap<Arc<str>, Node>,
        lp_arena: &mut Arena<ALogicalPlan>,
        expr_arena: &mut Arena<AExpr>,
    ) -> PolarsResult<ALogicalPlan> {
        let inputs = lp.get_inputs();
        let exprs = lp.get_exprs();

        let new_inputs = inputs
            .iter()
            .map(|&node| {
                let alp = lp_arena.take(node);
                let alp = self.push_down(
                    alp,
                    init_hashmap(Some(acc_predicates.len())),
                    lp_arena,
                    expr_arena,
                )?;
                lp_arena.replace(node, alp);
                Ok(node)
            })
            .collect::<PolarsResult<Vec<_>>>()?;
        let lp = lp.with_exprs_and_input(exprs, new_inputs);

        // all predicates are done locally
        let local_predicates = acc_predicates.values().copied().collect::<Vec<_>>();
        Ok(self.optional_apply_predicate(lp, local_predicates, lp_arena, expr_arena))
    }

    /// Predicate pushdown optimizer
    ///
    /// # Arguments
    ///
    /// * `AlogicalPlan` - Arena based logical plan tree representing the query.
    /// * `acc_predicates` - The predicates we accumulate during tree traversal.
    ///                      The hashmap maps from leaf-column name to predicates on that column.
    ///                      If the key is already taken we combine the predicate with a bitand operation.
    ///                      The `Node`s are indexes in the `expr_arena`
    /// * `lp_arena` - The local memory arena for the logical plan.
    /// * `expr_arena` - The local memory arena for the expressions.
    fn push_down(
        &self,
        lp: ALogicalPlan,
        mut acc_predicates: PlHashMap<Arc<str>, Node>,
        lp_arena: &mut Arena<ALogicalPlan>,
        expr_arena: &mut Arena<AExpr>,
    ) -> PolarsResult<ALogicalPlan> {
        use ALogicalPlan::*;

        match lp {
            Selection { predicate, input } => {

                // If a predicates result would be influenced by earlier applied filter
                // we remove it and apply it locally
                let local_predicates = transfer_to_local_by_node(&mut acc_predicates, |node| predicate_is_pushdown_boundary(node, expr_arena));

                insert_and_combine_predicate(&mut acc_predicates, predicate, expr_arena);
                let alp = lp_arena.take(input);
                let new_input = self.push_down(alp, acc_predicates, lp_arena, expr_arena)?;

                // TODO!
                // If a predicates result would be influenced by earlier applied
                // predicates, we simply don't pushdown this one passed this node
                // However, we can do better and let it pass but store the order of the predicates
                // so that we can apply them in correct order at the deepest level
                Ok(self.optional_apply_predicate(new_input, local_predicates, lp_arena, expr_arena))
            }
            DataFrameScan {
                df,
                schema,
                output_schema,
                projection,
                selection,
            } => {
                let selection = predicate_at_scan(acc_predicates, selection, expr_arena);
                let lp = DataFrameScan {
                    df,
                    schema,
                    output_schema,
                    projection,
                    selection,
                };
                Ok(lp)
            }

            LocalProjection { expr, input, .. } => {
                self.pushdown_and_assign(input, acc_predicates, lp_arena, expr_arena)?;

                let schema = lp_arena.get(input).schema(lp_arena);
                // projection from a wildcard may be dropped if the schema changes due to the optimization
                let expr: Vec<_> = expr
                    .into_iter()
                    .filter(|e| check_input_node(*e, &schema, expr_arena))
                    .collect();

                let schema = aexprs_to_schema(&expr, &schema, Context::Default, expr_arena);
                Ok(ALogicalPlan::LocalProjection {
                    expr,
                    input,
                    schema: Arc::new(schema),
                })
            }
            Scan {
                path,
                file_info,
                predicate,
                scan_type,
                file_options: options,
                output_schema
            } => {
                let local_predicates = partition_by_full_context(&mut acc_predicates, expr_arena);
                let predicate = predicate_at_scan(acc_predicates, predicate, expr_arena);

                let lp = match (predicate, &scan_type) {
                    #[cfg(feature = "csv")]
                    (Some(predicate), FileScan::Csv {..}) => {
                        let lp = Scan {
                            path,
                            file_info,
                            predicate: None,
                            file_options: options,
                            output_schema,
                            scan_type
                        };
                        let input = lp_arena.add(lp);
                        Selection {
                            input,
                            predicate
                        }
                    },
                    _ => {
                        Scan {
                            path,
                            file_info,
                            predicate,
                            file_options: options,
                            output_schema,
                            scan_type
                        }
                    }
                };

                Ok(self.optional_apply_predicate(lp, local_predicates, lp_arena, expr_arena))

            }
            AnonymousScan {
                function,
                file_info,
                output_schema,
                options,
                predicate,
            } => {
                if function.allows_predicate_pushdown() {
                    let local_predicates = partition_by_full_context(&mut acc_predicates, expr_arena);
                    let predicate = predicate_at_scan(acc_predicates, predicate, expr_arena);
                    let lp = AnonymousScan {
                        function,
                        file_info,
                        output_schema,
                        options,
                        predicate,
                    };
                    Ok(self.optional_apply_predicate(lp, local_predicates, lp_arena, expr_arena))
                } else {
                    let lp = AnonymousScan {
                        function,
                        file_info,
                        output_schema,
                        options,
                        predicate,
                    };
                    self.no_pushdown_restart_opt(lp, acc_predicates, lp_arena, expr_arena)
                }
            }

            Distinct {
                input,
                options
            } => {

                if matches!(options.keep_strategy, UniqueKeepStrategy::Any | UniqueKeepStrategy::None) {
                    // currently the distinct operation only keeps the first occurrences.
                    // this may have influence on the pushed down predicates. If the pushed down predicates
                    // contain a binary expression (thus depending on values in multiple columns)
                    // the final result may differ if it is pushed down.

                    let mut root_count = 0;

                    // if this condition is called more than once, its a binary or ternary operation.
                    let condition = |_| {
                        if root_count == 0 {
                            root_count += 1;
                            false
                        } else {
                            true
                        }
                    };
                    let mut local_predicates =
                        transfer_to_local_by_name(expr_arena, &mut acc_predicates, condition);
                    local_predicates.extend_from_slice(&transfer_to_local_by_node(&mut acc_predicates, |node| predicate_is_pushdown_boundary(node, expr_arena)));

                    self.pushdown_and_assign(input, acc_predicates, lp_arena, expr_arena)?;
                    let lp = Distinct {
                        input,
                        options
                    };
                    Ok(self.optional_apply_predicate(lp, local_predicates, lp_arena, expr_arena))
                } else {
                    let lp = Distinct {
                        input,
                        options
                    };
                    self.no_pushdown_restart_opt(lp, acc_predicates, lp_arena, expr_arena)
                }
            }
            Join {
                input_left,
                input_right,
                left_on,
                right_on,
                schema,
                options,
            } => {
                process_join(self, lp_arena,
                expr_arena,
                input_left,
                         input_right,
                         left_on,
                         right_on,
                         schema,
                         options,
                    acc_predicates
                )
            }
            MapFunction { ref function, .. } => {
                if function.allow_predicate_pd()
                {
                    match function {
                        FunctionNode::Rename {
                            existing,
                            new,
                            ..
                        } => {
                            let local_predicates = process_rename(&mut acc_predicates,
                             expr_arena,
                                existing,
                                new,
                            )?;
                            let lp = self.pushdown_and_continue(lp, acc_predicates, lp_arena, expr_arena, false)?;
                            Ok(self.optional_apply_predicate(lp, local_predicates, lp_arena, expr_arena))
                        },
                        FunctionNode::Explode {columns, ..} => {

                            let condition = |name: Arc<str>| columns.iter().any(|s| s.as_ref() == &*name);

                            // first columns that refer to the exploded columns should be done here
                            let mut local_predicates =
                                transfer_to_local_by_name(expr_arena, &mut acc_predicates, condition);

                            // if any predicate is a pushdown boundary, thus influenced by order of predicates e.g.: sum(), over(), sort
                            // we do all here. #5950
                            if acc_predicates.values().chain(local_predicates.iter()).any(|node| predicate_is_pushdown_boundary(*node, expr_arena)) {
                                local_predicates.extend(acc_predicates.drain().map(|(_name, node)| node))
                            }

                            let lp = self.pushdown_and_continue(lp, acc_predicates, lp_arena, expr_arena, false)?;
                            Ok(self.optional_apply_predicate(lp, local_predicates, lp_arena, expr_arena))

                        }
                        FunctionNode::Melt {
                            args,
                            ..
                        } => {

                            let variable_name = args.variable_name.as_deref().unwrap_or("variable");
                            let value_name = args.value_name.as_deref().unwrap_or("value");

                            // predicates that will be done at this level
                            let condition = |name: Arc<str>| {
                                let name = &*name;
                                name == variable_name
                                    || name == value_name
                                    || args.value_vars.iter().any(|s| s.as_str() == name)
                            };
                            let local_predicates =
                                transfer_to_local_by_name(expr_arena, &mut acc_predicates, condition);

                            let lp = self.pushdown_and_continue(lp, acc_predicates, lp_arena, expr_arena, false)?;
                            Ok(self.optional_apply_predicate(lp, local_predicates, lp_arena, expr_arena))

                        }
                        _ => {
                            self.pushdown_and_continue(lp, acc_predicates, lp_arena, expr_arena, false)
                        }
                    }


                } else {
                    self.no_pushdown_restart_opt(lp, acc_predicates, lp_arena, expr_arena)
                }
            }
            lp @ Union {..} => {
                let mut local_predicates = vec![];

                // a count is influenced by a Union/Vstack
                acc_predicates.retain(|_, predicate| {
                    if has_aexpr(*predicate, expr_arena, |ae| matches!(ae, AExpr::Count)) {
                        local_predicates.push(*predicate);
                        false
                    } else {
                        true
                    }
                });
                let lp = self.pushdown_and_continue(lp, acc_predicates, lp_arena, expr_arena, false)?;
                Ok(self.optional_apply_predicate(lp, local_predicates, lp_arena, expr_arena))
            }
            lp @ Sort{..} => {
                let mut local_predicates = vec![];
                acc_predicates.retain(|_, predicate| {
                    if predicate_is_sort_boundary(*predicate, expr_arena) {
                        local_predicates.push(*predicate);
                        false
                    } else {
                        true
                    }
                });
                let lp = self.pushdown_and_continue(lp, acc_predicates, lp_arena, expr_arena, false)?;
                Ok(self.optional_apply_predicate(lp, local_predicates, lp_arena, expr_arena))

            }
            // Pushed down passed these nodes
            lp@ FileSink {..} => {
                self.pushdown_and_continue(lp, acc_predicates, lp_arena, expr_arena, false)
            }
            #[cfg(feature = "cloud")]
            lp @ CloudSink { .. } => {
                self.pushdown_and_continue(lp, acc_predicates, lp_arena, expr_arena, false)
            }
            lp @ HStack {..} | lp @ Projection {..} | lp @ ExtContext {..} => {
                self.pushdown_and_continue(lp, acc_predicates, lp_arena, expr_arena, true)
            }
            // NOT Pushed down passed these nodes
            // predicates influence slice sizes
            lp @ Slice { .. }
            // caches will be different
            | lp @ Cache { .. }
            // dont push down predicates. An aggregation needs all rows
            | lp @ Aggregate {..} => {
                self.no_pushdown_restart_opt(lp, acc_predicates, lp_arena, expr_arena)
            }
            #[cfg(feature = "python")]
             PythonScan {mut options, predicate} => {
                if options.pyarrow {
                    let predicate = predicate_at_scan(acc_predicates, predicate, expr_arena);

                    if let Some(predicate) = predicate {
                        // simplify expressions before we translate them to pyarrow
                        let lp = PythonScan {options: options.clone(), predicate: Some(predicate)};
                        let lp_top = lp_arena.add(lp);
                        let stack_opt = StackOptimizer{};
                        let lp_top = stack_opt.optimize_loop(&mut [Box::new(SimplifyExprRule{})], expr_arena, lp_arena, lp_top).unwrap();
                        let PythonScan {options: _, predicate: Some(predicate)} = lp_arena.take(lp_top) else {unreachable!()};

                        match super::super::pyarrow::predicate_to_pa(predicate, expr_arena, Default::default()) {
                            // we we able to create a pyarrow string, mutate the options
                            Some(eval_str) => {
                                options.predicate = Some(eval_str)
                            },
                            // we were not able to translate the predicate
                            // apply here
                            None => {
                                let lp = PythonScan { options, predicate: None };
                                return Ok(self.optional_apply_predicate(lp, vec![predicate], lp_arena, expr_arena))
                            }
                        }
                    }
                    Ok(PythonScan {options, predicate})
                } else {
                    self.no_pushdown_restart_opt(PythonScan {options, predicate}, acc_predicates, lp_arena, expr_arena)
                }
            }

        }
    }

    pub fn optimize(
        &self,
        logical_plan: ALogicalPlan,
        lp_arena: &mut Arena<ALogicalPlan>,
        expr_arena: &mut Arena<AExpr>,
    ) -> PolarsResult<ALogicalPlan> {
        let acc_predicates = PlHashMap::new();
        self.push_down(logical_plan, acc_predicates, lp_arena, expr_arena)
    }
}
