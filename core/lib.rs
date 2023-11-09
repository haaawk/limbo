mod btree;
mod buffer_pool;
mod io;
mod pager;
mod schema;
mod sqlite3_ondisk;
mod storage;
mod translate;
mod types;
mod vdbe;

#[cfg(not(target_family = "wasm"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use anyhow::Result;
use fallible_iterator::FallibleIterator;
use pager::Pager;
use schema::Schema;
use sqlite3_parser::{ast::Cmd, lexer::sql::Parser};
use std::sync::Arc;

#[cfg(feature = "fs")]
pub use io::IO;
pub use io::{Buffer, Completion};
pub use storage::{Storage, StorageIO};
pub use types::Value;

pub struct Database {
    pager: Arc<Pager>,
    schema: Arc<Schema>,
}

impl Database {
    #[cfg(feature = "fs")]
    pub fn open_file(io: crate::io::IO, path: &str) -> Result<Database> {
        let file = io.open_file(path)?;
        let storage = storage::Storage::from_file(file);
        Self::open(storage)
    }

    pub fn open(storage: Storage) -> Result<Database> {
        let pager = Arc::new(Pager::open(storage)?);
        let bootstrap_schema = Arc::new(Schema::new());
        let conn = Connection {
            pager: pager.clone(),
            schema: bootstrap_schema.clone(),
        };
        let mut schema = Schema::new();
        let rows = conn.query("SELECT * FROM sqlite_schema")?;
        if let Some(mut rows) = rows {
            while let Some(row) = rows.next()? {
                let ty = row.get::<String>(0)?;
                if ty != "table" {
                    continue;
                }
                let name: String = row.get::<String>(1)?;
                let root_page: i64 = row.get::<i64>(3)?;
                let sql: String = row.get::<String>(4)?;
                let table = schema::Table::from_sql(&sql, root_page as usize)?;
                assert_eq!(table.name, name);
                schema.add_table(table.name.to_owned(), table);
            }
        }
        let schema = Arc::new(schema);
        Ok(Database { pager, schema })
    }

    pub fn connect(&self) -> Connection {
        Connection {
            pager: self.pager.clone(),
            schema: self.schema.clone(),
        }
    }
}

pub struct Connection {
    pager: Arc<Pager>,
    schema: Arc<Schema>,
}

impl Connection {
    pub fn prepare(&self, sql: impl Into<String>) -> Result<Statement> {
        let sql = sql.into();
        let mut parser = Parser::new(sql.as_bytes());
        let cmd = parser.next()?;
        if let Some(cmd) = cmd {
            match cmd {
                Cmd::Stmt(stmt) => {
                    let program = Arc::new(translate::translate(&self.schema, stmt)?);
                    Ok(Statement {
                        program,
                        pager: self.pager.clone(),
                    })
                }
                Cmd::Explain(_stmt) => todo!(),
                Cmd::ExplainQueryPlan(_stmt) => todo!(),
            }
        } else {
            todo!()
        }
    }

    pub fn query(&self, sql: impl Into<String>) -> Result<Option<Rows>> {
        let sql = sql.into();
        let mut parser = Parser::new(sql.as_bytes());
        let cmd = parser.next()?;
        if let Some(cmd) = cmd {
            match cmd {
                Cmd::Stmt(stmt) => {
                    let program = Arc::new(translate::translate(&self.schema, stmt)?);
                    let state = vdbe::ProgramState::new(program.max_registers);
                    Ok(Some(Rows::new(state, program, self.pager.clone())))
                }
                Cmd::Explain(stmt) => {
                    let program = translate::translate(&self.schema, stmt)?;
                    program.explain();
                    Ok(None)
                }
                Cmd::ExplainQueryPlan(_stmt) => Ok(None),
            }
        } else {
            Ok(None)
        }
    }

    pub fn execute(&self, sql: impl Into<String>) -> Result<()> {
        let sql = sql.into();
        let mut parser = Parser::new(sql.as_bytes());
        let cmd = parser.next()?;
        if let Some(cmd) = cmd {
            match cmd {
                Cmd::Explain(stmt) => {
                    let program = translate::translate(&self.schema, stmt)?;
                    program.explain();
                }
                Cmd::ExplainQueryPlan(_stmt) => todo!(),
                Cmd::Stmt(stmt) => {
                    let program = translate::translate(&self.schema, stmt)?;
                    let mut state = vdbe::ProgramState::new(program.max_registers);
                    program.step(&mut state, self.pager.clone())?;
                }
            }
        }
        Ok(())
    }
}

pub struct Statement {
    program: Arc<vdbe::Program>,
    pager: Arc<Pager>,
}

impl Statement {
    pub fn query(&self) -> Result<Rows> {
        let state = vdbe::ProgramState::new(self.program.max_registers);
        Ok(Rows::new(state, self.program.clone(), self.pager.clone()))
    }

    pub fn reset(&self) {}
}

pub struct Rows {
    state: vdbe::ProgramState,
    program: Arc<vdbe::Program>,
    pager: Arc<Pager>,
}

impl Rows {
    pub fn new(state: vdbe::ProgramState, program: Arc<vdbe::Program>, pager: Arc<Pager>) -> Self {
        Self {
            state,
            program,
            pager,
        }
    }

    pub fn next(&mut self) -> Result<Option<Row>> {
        loop {
            let result = self.program.step(&mut self.state, self.pager.clone())?;
            match result {
                vdbe::StepResult::Row(row) => {
                    return Ok(Some(Row { values: row.values }));
                }
                vdbe::StepResult::IO => todo!(),
                vdbe::StepResult::Done => {
                    return Ok(None);
                }
            }
        }
    }
}

pub struct Row {
    pub values: Vec<Value>,
}

impl Row {
    pub fn get<T: crate::types::FromValue>(&self, idx: usize) -> Result<T> {
        let value = &self.values[idx];
        T::from_value(value)
    }
}
