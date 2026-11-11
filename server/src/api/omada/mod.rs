//! Omada (TP-Link) controller proxy.
//!
//! Read-only data endpoints (VLANs, devices, switch ports, clients) and the
//! controller config endpoint, behind PDM's own TLS + auth.

pub mod http;
