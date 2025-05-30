mod if_stmt;
mod logic_expr;

use rspack_core::{CachedConstDependency, ConstDependency};
use swc_core::common::Spanned;

pub use self::logic_expr::is_logic_op;
use super::JavascriptParserPlugin;
use crate::{utils::eval::evaluate_to_string, visitors::JavascriptParser};

pub struct ConstPlugin;

const WEBPACK_RESOURCE_FRAGMENT: &str = "__resourceFragment";
const WEBPACK_RESOURCE_QUERY: &str = "__resourceQuery";

impl JavascriptParserPlugin for ConstPlugin {
  fn expression_logical_operator(
    &self,
    parser: &mut JavascriptParser,
    expr: &swc_core::ecma::ast::BinExpr,
  ) -> Option<bool> {
    self::logic_expr::expression_logic_operator(parser, expr)
  }

  fn expression_conditional_operation(
    &self,
    parser: &mut JavascriptParser,
    expression: &swc_core::ecma::ast::CondExpr,
  ) -> Option<bool> {
    let param = parser.evaluate_expression(&expression.test);
    if let Some(bool) = param.as_bool() {
      if !param.could_have_side_effects() {
        parser
          .presentational_dependencies
          .push(Box::new(ConstDependency::new(
            (param.range().0, param.range().1 - 1).into(),
            format!(" {bool}").into(),
            None,
          )));
      } else {
        parser.walk_expression(&expression.test);
      }
      if bool {
        parser
          .presentational_dependencies
          .push(Box::new(ConstDependency::new(
            expression.alt.span().into(),
            "0".into(),
            None,
          )));
      } else {
        parser
          .presentational_dependencies
          .push(Box::new(ConstDependency::new(
            expression.cons.span().into(),
            "0".into(),
            None,
          )));
      }
      Some(bool)
    } else {
      None
    }
  }

  fn statement_if(
    &self,
    parser: &mut JavascriptParser,
    expr: &swc_core::ecma::ast::IfStmt,
  ) -> Option<bool> {
    self::if_stmt::statement_if(parser, expr)
  }

  fn identifier(
    &self,
    parser: &mut JavascriptParser,
    ident: &swc_core::ecma::ast::Ident,
    for_name: &str,
  ) -> Option<bool> {
    match for_name {
      WEBPACK_RESOURCE_FRAGMENT => {
        let resource_fragment = parser
          .resource_data
          .resource_fragment
          .as_deref()
          .unwrap_or("");
        parser
          .presentational_dependencies
          .push(Box::new(CachedConstDependency::new(
            ident.span.into(),
            "__resourceFragment".into(),
            serde_json::to_string(resource_fragment)
              .expect("should render module id")
              .into(),
          )));
        Some(true)
      }
      WEBPACK_RESOURCE_QUERY => {
        let resource_query = parser.resource_data.resource_query.as_deref().unwrap_or("");
        parser
          .presentational_dependencies
          .push(Box::new(CachedConstDependency::new(
            ident.span.into(),
            "__resourceQuery".into(),
            serde_json::to_string(resource_query)
              .expect("should render module id")
              .into(),
          )));
        Some(true)
      }
      _ => None,
    }
  }

  fn evaluate_identifier(
    &self,
    parser: &mut JavascriptParser,
    ident: &str,
    start: u32,
    end: u32,
  ) -> Option<crate::utils::eval::BasicEvaluatedExpression<'static>> {
    match ident {
      WEBPACK_RESOURCE_QUERY => Some(evaluate_to_string(
        parser
          .resource_data
          .resource_query
          .clone()
          .unwrap_or_default(),
        start,
        end,
      )),
      WEBPACK_RESOURCE_FRAGMENT => Some(evaluate_to_string(
        parser
          .resource_data
          .resource_fragment
          .clone()
          .unwrap_or_default(),
        start,
        end,
      )),
      _ => None,
    }
  }
}
