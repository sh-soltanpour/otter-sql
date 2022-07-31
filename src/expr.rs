//! SQL expressions and their evaluation.

use std::{error::Error, fmt::Display};

use crate::{
    value::{Value, ValueError},
    BoundedString,
};

use sqlparser::ast::{self, ObjectName};

/// An expression
#[derive(Debug, Clone)]
pub enum Expr {
    Value(Value),
    ColumnRef(ObjectName),
    Wildcard,
    Binary {
        left: Box<Expr>,
        op: BinOp,
        right: Box<Expr>,
    },
    Unary {
        op: UnOp,
        operand: Box<Expr>,
    },
    Function {
        name: BoundedString,
        args: Vec<Expr>,
    },
}

/// A binary operator
#[derive(Debug, Copy, Clone)]
pub enum BinOp {
    Plus,
    Minus,
    Multiply,
    Divide,
    Modulo,
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
    Like,
    ILike,
    And,
    Or,
}

/// A unary operator
#[derive(Debug, Copy, Clone)]
pub enum UnOp {
    Plus,
    Minus,
    Not,
    IsFalse,
    IsTrue,
    IsNull,
    IsNotNull,
}

impl TryFrom<ast::Expr> for Expr {
    type Error = ExprError;
    fn try_from(value: ast::Expr) -> Result<Self, Self::Error> {
        match value {
            ast::Expr::Identifier(i) => Ok(Expr::ColumnRef(ObjectName(vec![i]))),
            ast::Expr::CompoundIdentifier(i) => Ok(Expr::ColumnRef(ObjectName(i))),
            ast::Expr::IsFalse(e) => Ok(Expr::Unary {
                op: UnOp::IsFalse,
                operand: Box::new((*e).try_into()?),
            }),
            ast::Expr::IsTrue(e) => Ok(Expr::Unary {
                op: UnOp::IsTrue,
                operand: Box::new((*e).try_into()?),
            }),
            ast::Expr::IsNull(e) => Ok(Expr::Unary {
                op: UnOp::IsNull,
                operand: Box::new((*e).try_into()?),
            }),
            ast::Expr::IsNotNull(e) => Ok(Expr::Unary {
                op: UnOp::IsNotNull,
                operand: Box::new((*e).try_into()?),
            }),
            ast::Expr::Between {
                expr,
                negated,
                low,
                high,
            } => {
                let expr: Box<Expr> = Box::new((*expr).try_into()?);
                let left = Box::new((*low).try_into()?);
                let right = Box::new((*high).try_into()?);
                let between = Expr::Binary {
                    left: Box::new(Expr::Binary {
                        left,
                        op: BinOp::LessThanOrEqual,
                        right: expr.clone(),
                    }),
                    op: BinOp::And,
                    right: Box::new(Expr::Binary {
                        left: expr,
                        op: BinOp::LessThanOrEqual,
                        right,
                    }),
                };
                if negated {
                    Ok(Expr::Unary {
                        op: UnOp::Not,
                        operand: Box::new(between),
                    })
                } else {
                    Ok(between)
                }
            }
            ast::Expr::BinaryOp { left, op, right } => Ok(Expr::Binary {
                left: Box::new((*left).try_into()?),
                op: op.try_into()?,
                right: Box::new((*right).try_into()?),
            }),
            ast::Expr::UnaryOp { op, expr } => Ok(Expr::Unary {
                op: op.try_into()?,
                operand: Box::new((*expr).try_into()?),
            }),
            ast::Expr::Value(v) => Ok(Expr::Value(v.try_into()?)),
            ast::Expr::Function(ref f) => Ok(Expr::Function {
                name: f.name.to_string().as_str().into(),
                args: f
                    .args
                    .iter()
                    .map(|arg| match arg {
                        ast::FunctionArg::Unnamed(arg_expr) => match arg_expr {
                            ast::FunctionArgExpr::Expr(e) => Ok(e.clone().try_into()?),
                            ast::FunctionArgExpr::Wildcard => Ok(Expr::Wildcard),
                            ast::FunctionArgExpr::QualifiedWildcard(_) => Err(ExprError::Expr {
                                reason: "Qualified wildcards are not supported yet",
                                expr: value.clone(),
                            }),
                        },
                        ast::FunctionArg::Named { .. } => Err(ExprError::Expr {
                            reason: "Named function arguments are not supported",
                            expr: value.clone(),
                        }),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            }),
            _ => Err(ExprError::Expr {
                reason: "Unsupported expression",
                expr: value,
            }),
        }
    }
}

impl TryFrom<ast::BinaryOperator> for BinOp {
    type Error = ExprError;
    fn try_from(op: ast::BinaryOperator) -> Result<Self, Self::Error> {
        match op {
            ast::BinaryOperator::Plus => Ok(BinOp::Plus),
            ast::BinaryOperator::Minus => Ok(BinOp::Minus),
            ast::BinaryOperator::Multiply => Ok(BinOp::Multiply),
            ast::BinaryOperator::Divide => Ok(BinOp::Divide),
            ast::BinaryOperator::Modulo => Ok(BinOp::Modulo),
            ast::BinaryOperator::Eq => Ok(BinOp::Equal),
            ast::BinaryOperator::NotEq => Ok(BinOp::NotEqual),
            ast::BinaryOperator::Lt => Ok(BinOp::LessThan),
            ast::BinaryOperator::LtEq => Ok(BinOp::LessThanOrEqual),
            ast::BinaryOperator::Gt => Ok(BinOp::GreaterThan),
            ast::BinaryOperator::GtEq => Ok(BinOp::GreaterThanOrEqual),
            ast::BinaryOperator::Like => Ok(BinOp::Like),
            ast::BinaryOperator::ILike => Ok(BinOp::ILike),
            ast::BinaryOperator::And => Ok(BinOp::And),
            ast::BinaryOperator::Or => Ok(BinOp::Or),
            // TODO: xor?
            _ => Err(ExprError::Binary {
                reason: "Unknown binary operator",
                op,
            }),
        }
    }
}

impl TryFrom<ast::UnaryOperator> for UnOp {
    type Error = ExprError;
    fn try_from(op: ast::UnaryOperator) -> Result<Self, Self::Error> {
        match op {
            ast::UnaryOperator::Plus => Ok(UnOp::Plus),
            ast::UnaryOperator::Minus => Ok(UnOp::Minus),
            ast::UnaryOperator::Not => Ok(UnOp::Not),
            // IsFalse, IsTrue, etc. are handled in TryFrom<ast::Expr> for Expr
            // since `sqlparser` does not consider them unary operators for some reason.
            _ => Err(ExprError::Unary {
                reason: "Unkown unary operator",
                op,
            }),
        }
    }
}

#[derive(Debug)]
pub enum ExprError {
    Expr {
        reason: &'static str,
        expr: ast::Expr,
    },
    Binary {
        reason: &'static str,
        op: ast::BinaryOperator,
    },
    Unary {
        reason: &'static str,
        op: ast::UnaryOperator,
    },
    Value(ValueError),
}

impl Display for ExprError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ExprError::Expr { reason, expr } => {
                write!(f, "ExprError: {}: {}", reason, expr)
            }
            ExprError::Binary { reason, op } => {
                write!(f, "ExprError: {}: {}", reason, op)
            }
            ExprError::Unary { reason, op } => {
                write!(f, "ExprError: {}: {}", reason, op)
            }
            ExprError::Value(v) => write!(f, "{}", v),
        }
    }
}

impl From<ValueError> for ExprError {
    fn from(v: ValueError) -> Self {
        Self::Value(v)
    }
}

impl Error for ExprError {}
