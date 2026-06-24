//! Admin API 模块
//!
//! 提供凭据管理和监控功能的 HTTP API
//!
//! # 功能
//! - 查询所有凭据状态
//! - 启用/禁用凭据
//! - 修改凭据优先级
//! - 重置失败计数
//! - 查询凭据余额
//!
//! # 使用
//! ```ignore
//! let admin_service = AdminService::new(token_manager.clone(), endpoint_names);
//! let admin_state = AdminState::new(admin_api_key, admin_service);
//! let admin_router = create_admin_router(admin_state);
//! ```

mod binary_update;
pub mod client_keys;
mod error;
pub mod groups;
mod handlers;
mod middleware;
pub mod proxy_pool;
mod router;
mod service;
pub mod trace_db;
pub mod types;
pub mod usage_stats;

pub use client_keys::ClientKeyManager;
pub use groups::GroupManager;
pub use middleware::AdminState;
pub use router::create_admin_router;
pub use service::AdminService;
pub use trace_db::{SharedTraceStore, TraceStore};
pub use usage_stats::{UsageAggregator, UsageRecorder};
