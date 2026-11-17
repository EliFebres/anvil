//! The factory's own types.
//!
//! `factory.toml` (its schema, template and validators), the `~/.factory`
//! env file and the git arguments the factory passes to reach GitHub with a
//! token. Like `anvil-types`, the only logic here is validation and loading;
//! the crate never depends on tokio or reqwest. Types the factory shares with
//! anvil (the ticket, the anvil configuration) stay in `anvil-types`.

pub mod dotenv;
pub mod error;
pub mod factory_config;
pub mod github;

pub use error::ValidationError;
pub use factory_config::{
    BudgetConfig, ContainerRuntime, EvalConfig, FactoryConfig, FactoryRoles, GithubConfig,
    NotifyConfig, ProtectedConfig, Reviewer, ServiceConfig, Stance, WorkerConfig,
};
