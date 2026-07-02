//! Consolidated offline integration tests for the knowledge crate.

#[path = "knowledge_offline/chunking.rs"]
mod chunking;
#[path = "knowledge_offline/native_parser.rs"]
mod native_parser;
#[path = "knowledge_offline/parser_llamaparse.rs"]
mod parser_llamaparse;
#[path = "knowledge_offline/parser_reducto.rs"]
mod parser_reducto;
#[path = "knowledge_offline/parser_unstructured.rs"]
mod parser_unstructured;
#[path = "knowledge_offline/provider_merge.rs"]
mod provider_merge;
#[path = "knowledge_offline/provider_nango.rs"]
mod provider_nango;
