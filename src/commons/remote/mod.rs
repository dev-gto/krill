//! This module provides support for proxying RFC compliant clients, using XML in
//! CMS to the krill native HTTPS JSON API.
pub mod api;
pub mod builder;
pub mod clients;
pub mod id;
pub mod proxy;
pub mod responder;
pub mod rfc6492;
pub mod rfc8181;
pub mod rfc8183;
pub mod sigmsg;
