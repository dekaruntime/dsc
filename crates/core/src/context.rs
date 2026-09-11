use std::path::PathBuf;

use crate::args::{ParseError, parse_env};
use crate::args::Args;
use crate::registry::Registry;

#[derive(Debug, Clone)]
pub struct Context {
    pub args: Args,
    pub env: EnvContext,
}

#[derive(Debug, Clone)]
pub struct EnvContext {
    pub cwd: PathBuf,
}

#[derive(Debug, Clone)]
pub enum ContextError {
    Parse(Vec<ParseError>),
}

impl EnvContext {
    pub fn load() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self { cwd }
    }
}

impl Context {
    pub fn from_env(registry: &Registry) -> Result<Self, ContextError> {
        let parsed = parse_env(registry);
        if !parsed.errors.is_empty() {
            return Err(ContextError::Parse(parsed.errors));
        }

        Ok(Self {
            args: parsed.args,
            env: EnvContext::load(),
        })
    }
}
