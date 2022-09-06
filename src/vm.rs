use std::collections::HashMap;
use std::error::Error;
use std::fmt::Display;

use sqlparser::parser::ParserError;

use crate::codegen::{codegen, CodegenError};
use crate::column::Column;
use crate::expr::eval::ExprExecError;
use crate::expr::Expr;
use crate::ic::{Instruction, IntermediateCode};
use crate::identifier::TableRef;
use crate::parser::parse;
use crate::schema::Schema;
use crate::table::{Row, Table};
use crate::value::Value;
use crate::{BoundedString, Database, Mrc};

const DEFAULT_DATABASE_NAME: &str = "default";

/// An index that can be used to access a specific register.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct RegisterIndex(usize);

impl RegisterIndex {
    /// Get the next index in the sequence.
    pub fn next_index(&self) -> RegisterIndex {
        RegisterIndex(self.0 + 1)
    }
}

impl Display for RegisterIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "%{}", self.0)
    }
}

/// An index that can be used as a reference to a table.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct TableIndex(usize);

impl TableIndex {
    /// Get the next index in the sequence.
    pub fn next_index(&self) -> Self {
        TableIndex(self.0 + 1)
    }
}

/// Executor of an SQL query.
pub struct VirtualMachine {
    database: Database,
    registers: HashMap<RegisterIndex, Register>,
    tables: HashMap<TableIndex, Table>,
    last_table_index: TableIndex,
}

impl VirtualMachine {
    pub fn new(name: BoundedString) -> Self {
        Self {
            database: Database::new(name),
            registers: Default::default(),
            tables: Default::default(),
            last_table_index: Default::default(),
        }
    }

    /// Inserts a value for the register at the given index.
    pub fn insert_register(&mut self, index: RegisterIndex, reg: Register) {
        self.registers.insert(index.clone(), reg);
    }

    /// Gets the value for the register at the given index.
    pub fn get_register(&mut self, index: &RegisterIndex) -> Option<&Register> {
        self.registers.get(index)
    }

    /// Creates a new table with a temp name and returns its index.
    pub fn new_temp_table(&mut self) -> TableIndex {
        let index = self.last_table_index.next_index();
        self.tables.insert(index, Table::new_temp(index.0));
        index
    }

    /// Get a reference to an existing table at the given index.
    pub fn table(&self, index: &TableIndex) -> Option<&Table> {
        self.tables.get(index)
    }

    /// Drop an existing table from the VM.
    ///
    /// Note: does NOT remove the table from the schema (if it was added to a schema).
    // TODO: ensure that IC gen calls this when a temp table is created.
    pub fn drop_table(&mut self, index: &TableIndex) {
        self.tables.remove(index);
    }

    /// Executes the given SQL.
    pub fn execute(&mut self, code: &str) -> Result<Option<Table>, ExecutionError> {
        let ast = parse(code)?;
        let mut ret = None;
        for stmt in ast {
            let ic = codegen(&stmt)?;
            ret = self.execute_ic(&ic)?;
        }
        Ok(ret)
    }

    /// Executes the given intermediate code.
    fn execute_ic(&mut self, ic: &IntermediateCode) -> Result<Option<Table>, RuntimeError> {
        let mut ret = None;
        for instr in &ic.instrs {
            ret = self.execute_instr(instr)?;
        }
        Ok(ret)
    }

    /// Executes the given instruction.
    fn execute_instr(&mut self, instr: &Instruction) -> Result<Option<Table>, RuntimeError> {
        let _ = &self.database;
        match instr {
            Instruction::Value { index, value } => {
                self.registers
                    .insert(*index, Register::Value(value.clone()));
            }
            Instruction::Expr { index, expr } => {
                self.registers.insert(*index, Register::Expr(expr.clone()));
            }
            Instruction::Source { index, name } => match name {
                TableRef {
                    schema_name: None,
                    table_name,
                } => {
                    let table_index =
                        self.find_table(self.database.default_schema(), name, table_name)?;
                    self.registers
                        .insert(*index, Register::TableRef(table_index));
                }
                TableRef {
                    schema_name: Some(schema_name),
                    table_name,
                } => {
                    let schema = if let Some(schema) = self.database.schema_by_name(schema_name) {
                        schema
                    } else {
                        return Err(RuntimeError::SchemaNotFound(*schema_name));
                    };

                    let table_index = self.find_table(schema, name, table_name)?;
                    self.registers
                        .insert(*index, Register::TableRef(table_index));
                }
            },
            Instruction::Empty { index } => {
                let table_index = self.new_temp_table();
                self.registers
                    .insert(*index, Register::TableRef(table_index));
            }
            Instruction::Return { index } => match self.registers.remove(index) {
                None => return Err(RuntimeError::EmptyRegister(*index)),
                Some(Register::TableRef(t)) => return Ok(Some(self.tables[&t].clone())),
                Some(Register::Value(v)) => {
                    let mut table = Table::new_temp(self.last_table_index.next_index().0);
                    table.add_column(Column::new("?column?".into(), v.data_type(), vec![], false));
                    table.new_row(vec![v]);
                    return Ok(Some(table));
                }
                Some(register) => return Err(RuntimeError::CannotReturn(register.clone())),
            },
            Instruction::Filter { index, expr } => match self.registers.get(index) {
                None => return Err(RuntimeError::EmptyRegister(*index)),
                Some(Register::TableRef(table_index)) => {
                    // TODO: should be safe to unwrap, but make it an error anyway?
                    let table = self.tables.get(table_index).unwrap();
                    let filtered_data = table
                        .raw_data
                        .iter()
                        .filter_map(|row| match Expr::execute(expr, &self, table, row) {
                            Ok(val) => match val {
                                Value::Bool(b) => {
                                    if b {
                                        Some(Ok(row.clone()))
                                    } else {
                                        None
                                    }
                                }
                                _ => Some(Err(RuntimeError::FilterWithNonBoolean(
                                    expr.clone(),
                                    val.clone(),
                                ))),
                            },
                            Err(e) => Some(Err(e.into())),
                        })
                        .collect::<Result<_, _>>()?;
                    self.tables.get_mut(table_index).unwrap().raw_data = filtered_data;
                }
                Some(reg) => return Err(RuntimeError::RegisterNotATable("filter", reg.clone())),
            },
        }
        Ok(None)
    }

    /// Find [`TableIndex`] given the schema and its name.
    fn find_table(
        &self,
        schema: &Schema,
        table: &TableRef,
        table_name: &BoundedString,
    ) -> Result<TableIndex, RuntimeError> {
        if let Some(table_index) = schema
            .tables()
            .iter()
            .find(|table_index| self.tables[table_index].name() == table_name)
        {
            Ok(*table_index)
        } else {
            Err(RuntimeError::TableNotFound(table.clone()))
        }
    }
}

impl Default for VirtualMachine {
    fn default() -> Self {
        Self::new(DEFAULT_DATABASE_NAME.into())
    }
}

#[derive(Debug, Clone)]
/// A register in the executor VM.
pub enum Register {
    /// A reference to a table.
    TableRef(TableIndex),
    /// A grouped table.
    GroupedTable {
        grouped_col: Column,
        other_cols: Vec<Column>,
        /// The group, a mapping of grouped col value -> rows in that group.
        data: Vec<(Value, Vec<Row>)>,
    },
    /// A table definition.
    TableDef(TableDef),
    /// A column definition
    Column(Column),
    /// An insert statement
    InsertDef(InsertDef),
    /// A row to insert
    InsertRow(InsertRow),
    /// A value
    Value(Value),
    /// An expression
    Expr(Expr),
    // TODO: an error value?
}

#[derive(Debug, Clone)]
/// An abstract definition of a create table statement.
pub struct TableDef {
    pub name: BoundedString,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone)]
/// An abstract definition of an insert statement.
pub struct InsertDef {
    /// The view to insert into
    pub table_name: BoundedString,
    /// The columns to insert into.
    ///
    /// Empty means all columns.
    pub columns: Vec<Column>,
    /// The values to insert.
    pub rows: Vec<InsertRow>,
}

impl InsertDef {
    pub fn new(table_name: BoundedString) -> Self {
        Self {
            table_name,
            columns: Vec::new(),
            rows: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
/// A row of values to insert.
pub struct InsertRow {
    /// The values
    pub values: Vec<Value>,
    /// The insert definition which this belongs to
    pub def: Mrc<InsertDef>,
}

#[derive(Debug)]
pub enum ExecutionError {
    ParseError(ParserError),
    CodegenError(CodegenError),
    RuntimeError(RuntimeError),
}

impl From<ParserError> for ExecutionError {
    fn from(err: ParserError) -> Self {
        ExecutionError::ParseError(err)
    }
}

impl From<CodegenError> for ExecutionError {
    fn from(err: CodegenError) -> Self {
        ExecutionError::CodegenError(err)
    }
}

impl From<RuntimeError> for ExecutionError {
    fn from(err: RuntimeError) -> Self {
        ExecutionError::RuntimeError(err)
    }
}

impl Display for ExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParseError(e) => write!(f, "{}", e),
            Self::CodegenError(e) => write!(f, "{}", e),
            Self::RuntimeError(e) => write!(f, "{}", e),
        }
    }
}

impl Error for ExecutionError {}

#[derive(Debug)]
pub enum RuntimeError {
    TableNotFound(TableRef),
    SchemaNotFound(BoundedString),
    EmptyRegister(RegisterIndex),
    RegisterNotATable(&'static str, Register),
    CannotReturn(Register),
    FilterWithNonBoolean(Expr, Value),
    ExprExecError(ExprExecError),
}

impl From<ExprExecError> for RuntimeError {
    fn from(e: ExprExecError) -> Self {
        Self::ExprExecError(e)
    }
}

impl Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TableNotFound(t) => write!(f, "Table not found: '{}'", t),
            Self::SchemaNotFound(s) => write!(f, "Schema not found: '{}'", s),
            Self::EmptyRegister(r) => write!(
                f,
                "Register is not initialized: '{}' (critical error. Please file an issue.)",
                r
            ),
            Self::RegisterNotATable(operation, reg) => write!(
                f,
                "Register is not a table. Cannot perform '{}' on '{:?}'",
                operation, reg
            ),
            Self::CannotReturn(r) => write!(
                f,
                "Register value cannot be returned: '{:?}' (critical error. Please file an issue)",
                r
            ),
            Self::FilterWithNonBoolean(e, v) => write!(
                f,
                "WHERE clause used with a non-boolean value. Expression: '{}' evaluated to value: '{}'",
                e, v
            ),
            Self::ExprExecError(e) => write!(f, "{}", e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::VirtualMachine;

    #[test]
    fn create_vm() {
        let _ = VirtualMachine::default();
    }
}
