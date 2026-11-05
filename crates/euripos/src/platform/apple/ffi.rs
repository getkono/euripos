//! The parts of Network.framework's and libdispatch's C API the Apple
//! platform calls, declared by hand (docs/adr/0003-apple-network-framework.md,
//! "Bindings").
//!
//! Every Network.framework object is an opaque reference-counted pointer,
//! released with [`nw_release`]; a dispatch object is released with
//! [`dispatch_release`]. Callbacks are blocks, whose ABI `block2` implements.
//! An object passed to a callback is borrowed for the call.

#![allow(
    non_camel_case_types,
    reason = "the C API's own names, so each declaration can be checked against the SDK headers"
)]

use std::ffi::{c_char, c_int, c_void};

use block2::Block;

pub(crate) type nw_object_t = *mut c_void;
pub(crate) type nw_endpoint_t = nw_object_t;
pub(crate) type nw_parameters_t = nw_object_t;
pub(crate) type nw_protocol_options_t = nw_object_t;
pub(crate) type nw_connection_t = nw_object_t;
pub(crate) type nw_content_context_t = nw_object_t;
pub(crate) type nw_error_t = nw_object_t;
pub(crate) type nw_path_monitor_t = nw_object_t;
pub(crate) type nw_path_t = nw_object_t;
pub(crate) type nw_interface_t = nw_object_t;
pub(crate) type dispatch_queue_t = *mut c_void;
pub(crate) type dispatch_data_t = *mut c_void;

/// C's `bool` as a block's argument or result. `block2` takes no Rust `bool`
/// there; on the aarch64 targets euripos supports both are one byte holding
/// 0 or 1, passed in the low byte of a register.
pub(crate) type CBool = u8;

/// `nw_parameters_configure_protocol_block_t`.
pub(crate) type ConfigureProtocol = Block<dyn Fn(nw_protocol_options_t)>;

/// `nw_connection_state_t`.
pub(crate) const NW_CONNECTION_STATE_WAITING: u32 = 1;
pub(crate) const NW_CONNECTION_STATE_READY: u32 = 3;
pub(crate) const NW_CONNECTION_STATE_FAILED: u32 = 4;
pub(crate) const NW_CONNECTION_STATE_CANCELLED: u32 = 5;

/// `nw_error_domain_t`.
pub(crate) const NW_ERROR_DOMAIN_POSIX: u32 = 1;
pub(crate) const NW_ERROR_DOMAIN_DNS: u32 = 2;

/// `nw_path_status_t`.
pub(crate) const NW_PATH_STATUS_SATISFIED: u32 = 1;

/// `nw_interface_type_t`.
pub(crate) const NW_INTERFACE_TYPE_WIFI: u32 = 1;
pub(crate) const NW_INTERFACE_TYPE_CELLULAR: u32 = 2;
pub(crate) const NW_INTERFACE_TYPE_WIRED: u32 = 3;

/// `nw_endpoint_type_t`.
pub(crate) const NW_ENDPOINT_TYPE_ADDRESS: u32 = 1;

#[link(name = "Network", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn nw_release(object: nw_object_t);

    pub(crate) fn nw_endpoint_create_host(
        hostname: *const c_char,
        port: *const c_char,
    ) -> nw_endpoint_t;
    pub(crate) fn nw_endpoint_get_type(endpoint: nw_endpoint_t) -> u32;
    pub(crate) fn nw_endpoint_get_address(endpoint: nw_endpoint_t) -> *const libc::sockaddr;

    /// `NW_PARAMETERS_DEFAULT_CONFIGURATION`.
    pub(crate) static _nw_parameters_configure_protocol_default_configuration:
        &'static ConfigureProtocol;
    /// `NW_PARAMETERS_DISABLE_PROTOCOL`.
    pub(crate) static _nw_parameters_configure_protocol_disable: &'static ConfigureProtocol;
    pub(crate) fn nw_parameters_create_secure_tcp(
        configure_tls: &ConfigureProtocol,
        configure_tcp: &ConfigureProtocol,
    ) -> nw_parameters_t;

    pub(crate) fn nw_connection_create(
        endpoint: nw_endpoint_t,
        parameters: nw_parameters_t,
    ) -> nw_connection_t;
    pub(crate) fn nw_connection_set_queue(connection: nw_connection_t, queue: dispatch_queue_t);
    pub(crate) fn nw_connection_set_state_changed_handler(
        connection: nw_connection_t,
        handler: &Block<dyn Fn(u32, nw_error_t)>,
    );
    pub(crate) fn nw_connection_start(connection: nw_connection_t);
    pub(crate) fn nw_connection_cancel(connection: nw_connection_t);
    pub(crate) fn nw_connection_receive(
        connection: nw_connection_t,
        minimum_incomplete_length: u32,
        maximum_length: u32,
        completion: &Block<dyn Fn(dispatch_data_t, nw_content_context_t, CBool, nw_error_t)>,
    );
    pub(crate) fn nw_connection_send(
        connection: nw_connection_t,
        content: dispatch_data_t,
        context: nw_content_context_t,
        is_complete: bool,
        completion: &Block<dyn Fn(nw_error_t)>,
    );

    /// `NW_CONNECTION_DEFAULT_STREAM_CONTEXT`.
    pub(crate) static _nw_content_context_default_stream: nw_content_context_t;
    /// `NW_CONNECTION_FINAL_MESSAGE_CONTEXT`.
    pub(crate) static _nw_content_context_final_send: nw_content_context_t;

    pub(crate) fn nw_error_get_error_domain(error: nw_error_t) -> u32;
    pub(crate) fn nw_error_get_error_code(error: nw_error_t) -> c_int;

    pub(crate) fn nw_path_monitor_create() -> nw_path_monitor_t;
    pub(crate) fn nw_path_monitor_set_queue(monitor: nw_path_monitor_t, queue: dispatch_queue_t);
    pub(crate) fn nw_path_monitor_set_update_handler(
        monitor: nw_path_monitor_t,
        handler: &Block<dyn Fn(nw_path_t)>,
    );
    pub(crate) fn nw_path_monitor_start(monitor: nw_path_monitor_t);
    pub(crate) fn nw_path_monitor_cancel(monitor: nw_path_monitor_t);

    pub(crate) fn nw_path_get_status(path: nw_path_t) -> u32;
    pub(crate) fn nw_path_is_expensive(path: nw_path_t) -> bool;
    pub(crate) fn nw_path_is_constrained(path: nw_path_t) -> bool;
    pub(crate) fn nw_path_has_ipv4(path: nw_path_t) -> bool;
    pub(crate) fn nw_path_has_ipv6(path: nw_path_t) -> bool;
    pub(crate) fn nw_path_enumerate_interfaces(
        path: nw_path_t,
        enumerator: &Block<dyn Fn(nw_interface_t) -> CBool + '_>,
    );
    pub(crate) fn nw_path_enumerate_gateways(
        path: nw_path_t,
        enumerator: &Block<dyn Fn(nw_endpoint_t) -> CBool + '_>,
    );

    pub(crate) fn nw_interface_get_type(interface: nw_interface_t) -> u32;
    pub(crate) fn nw_interface_get_index(interface: nw_interface_t) -> u32;
}

// libdispatch is part of libSystem, which every binary links.
unsafe extern "C" {
    pub(crate) fn dispatch_queue_create(
        label: *const c_char,
        attributes: *mut c_void,
    ) -> dispatch_queue_t;
    pub(crate) fn dispatch_release(object: *mut c_void);
    /// With a null `destructor` (`DISPATCH_DATA_DESTRUCTOR_DEFAULT`) the
    /// bytes are copied before it returns.
    pub(crate) fn dispatch_data_create(
        buffer: *const c_void,
        size: usize,
        queue: dispatch_queue_t,
        destructor: *const c_void,
    ) -> dispatch_data_t;
    pub(crate) fn dispatch_data_create_map(
        data: dispatch_data_t,
        buffer: *mut *const c_void,
        size: *mut usize,
    ) -> dispatch_data_t;
}
