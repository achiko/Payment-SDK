//! Backend-independent atomic storage contract.

mod batch;
mod error;
mod key;
mod storage;

pub use batch::{Condition, Operation, WriteBatch};
pub use error::{Error, ErrorKind};
pub use key::{Key, Namespace, StoredValue, Value, Version};
pub use storage::{CommitResult, ScanPage, ScanRequest, Store};

use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
