//! rsou 核心库:文档解析与全文检索的纯逻辑层。
//!
//! 独立于 GUI(不依赖 egui),便于单元测试与 CLI 复用;GUI 与 CLI 都通过
//! `store::open` 打开索引库,保证自定义 FTS5 tokenizer 的注册只有一个入口。

pub mod chunk;
pub mod filebrowser;
pub mod import;
pub mod maintain;
pub mod parse;
pub mod query;
pub mod repo;
pub mod search;
pub mod store;
pub mod text;
pub mod tokenize;
