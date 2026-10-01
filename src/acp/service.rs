//! Compatibility exports for existing ACP service consumers.
//! Interactive control is owned by the product domain, independently of ACP.
pub use crate::interactive::{
    InteractiveService as EditorService, InteractiveSession as EditorSession, ServiceConfig,
};
