use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::capability_gate::require;
use seeed_hal_core::{
    CapabilityId, CapabilitySet, ErrorCategory, HalError, HalResult, LeaseToken, OwnerId,
    ResourceDescriptor, ResourceId, ResourceSelector, SessionId,
};
use seeed_hal_gpio::{
    GPIO_EDGES_CAPABILITY, GPIO_LINES_CAPABILITY, MAX_GPIO_EVENTS, gpio_edges_capability,
    gpio_lines_capability,
};
use seeed_hal_protocol::v1::{self, envelope};
use seeed_hal_protocol::{
    gpio_close_request_from_proto, gpio_next_edge_request_from_proto, gpio_read_request_from_proto,
    gpio_read_response_to_proto, gpio_write_request_from_proto, invalid_message,
    open_gpio_request_from_proto, open_gpio_response_to_proto, open_usb_request_from_proto,
    open_usb_response_to_proto, usb_close_request_from_proto, usb_transfer_request_from_proto,
    usb_transfer_response_to_proto,
};
use seeed_hal_runtime::HalRuntime;
use seeed_hal_usb::{
    USB_BULK_CAPABILITY, USB_CONTROL_CAPABILITY, USB_INTERRUPT_CAPABILITY, UsbTransfer,
    usb_bulk_capability, usb_control_capability, usb_interrupt_capability,
};

pub(crate) const USB_GPIO_WIRE_MINOR: u32 = 2;
const CLOSED_SESSION_RETENTION: usize = 256;

#[derive(Clone, Copy)]
pub(crate) struct UsbGpioDispatchLimits {
    pub(crate) max_frame_bytes: usize,
    pub(crate) max_read_bytes: usize,
    pub(crate) max_write_bytes: usize,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SessionKind {
    Usb,
    Gpio,
}

struct SessionRecord {
    resource_id: ResourceId,
    capabilities: CapabilitySet,
    lease: LeaseToken,
    kind: SessionKind,
    closed: bool,
}

#[derive(Default)]
pub(crate) struct UsbGpioSessionRegistry {
    sessions: HashMap<SessionId, SessionRecord>,
    closed_order: VecDeque<SessionId>,
}

pub(crate) type UsbGpioSessions = Arc<Mutex<UsbGpioSessionRegistry>>;

pub(crate) fn new_session_registry() -> UsbGpioSessions {
    Arc::new(Mutex::new(UsbGpioSessionRegistry::default()))
}

pub(crate) fn broker_capabilities(protocol_minor: u32) -> Vec<String> {
    if protocol_minor < USB_GPIO_WIRE_MINOR {
        return Vec::new();
    }
    [
        USB_CONTROL_CAPABILITY,
        USB_BULK_CAPABILITY,
        USB_INTERRUPT_CAPABILITY,
        GPIO_LINES_CAPABILITY,
        GPIO_EDGES_CAPABILITY,
    ]
    .map(str::to_owned)
    .into()
}

pub(crate) async fn dispatch(
    runtime: HalRuntime,
    owner: OwnerId,
    payload: envelope::Payload,
    limits: UsbGpioDispatchLimits,
    sessions: UsbGpioSessions,
) -> HalResult<envelope::Payload> {
    match payload {
        envelope::Payload::EnumerateUsbRequest(_) => enumerate_usb(runtime).await,
        envelope::Payload::OpenUsbRequest(request) => {
            open_usb(runtime, owner, request, sessions).await
        }
        envelope::Payload::UsbTransferRequest(request) => {
            transfer(runtime, request, limits, sessions).await
        }
        envelope::Payload::CloseUsbRequest(request) => close_usb(runtime, request, sessions).await,
        envelope::Payload::EnumerateGpioRequest(_) => enumerate_gpio(runtime).await,
        envelope::Payload::OpenGpioRequest(request) => {
            open_gpio(runtime, owner, request, sessions).await
        }
        envelope::Payload::GpioReadRequest(request) => read_gpio(runtime, request, sessions).await,
        envelope::Payload::GpioWriteRequest(request) => {
            write_gpio(runtime, request, limits, sessions).await
        }
        envelope::Payload::GpioNextEdgeRequest(request) => {
            next_edge(runtime, request, limits, sessions).await
        }
        envelope::Payload::CloseGpioRequest(request) => {
            close_gpio(runtime, request, sessions).await
        }
        _ => Err(invalid_message(
            "USB/GPIO response payloads are not valid client requests",
        )),
    }
}

async fn enumerate_usb(runtime: HalRuntime) -> HalResult<envelope::Payload> {
    Ok(envelope::Payload::EnumerateUsbResponse(
        v1::EnumerateUsbResponse {
            resources: runtime
                .enumerate_usb()
                .await?
                .iter()
                .map(TryInto::try_into)
                .collect::<HalResult<Vec<_>>>()?,
        },
    ))
}

async fn enumerate_gpio(runtime: HalRuntime) -> HalResult<envelope::Payload> {
    Ok(envelope::Payload::EnumerateGpioResponse(
        v1::EnumerateGpioResponse {
            resources: runtime
                .enumerate_gpio()
                .await?
                .iter()
                .map(TryInto::try_into)
                .collect::<HalResult<Vec<_>>>()?,
        },
    ))
}

async fn open_usb(
    runtime: HalRuntime,
    owner: OwnerId,
    request: v1::OpenUsbRequest,
    sessions: UsbGpioSessions,
) -> HalResult<envelope::Payload> {
    let (selector, interface) = open_usb_request_from_proto(request)?;
    let descriptors = runtime.enumerate_usb().await?;
    let descriptor = select(&descriptors, &selector, "usb.open")?;
    require(
        descriptor.capabilities(),
        &usb_control_capability(),
        USB_CONTROL_CAPABILITY,
        descriptor.id(),
    )?;
    let handle = runtime.open_usb(owner, selector, interface).await?;
    let (session, lease) = handle.into_parts();
    record(
        &sessions,
        session.clone(),
        descriptor,
        lease.clone(),
        SessionKind::Usb,
    );
    Ok(envelope::Payload::OpenUsbResponse(
        open_usb_response_to_proto(&session, &lease),
    ))
}

async fn open_gpio(
    runtime: HalRuntime,
    owner: OwnerId,
    request: v1::OpenGpioRequest,
    sessions: UsbGpioSessions,
) -> HalResult<envelope::Payload> {
    let (selector, lines, config) = open_gpio_request_from_proto(request)?;
    let descriptors = runtime.enumerate_gpio().await?;
    let descriptor = select(&descriptors, &selector, "gpio.open")?;
    require(
        descriptor.capabilities(),
        &gpio_lines_capability(),
        GPIO_LINES_CAPABILITY,
        descriptor.id(),
    )?;
    let handle = runtime.open_gpio(owner, selector, lines, config).await?;
    let (session, lease) = handle.into_parts();
    record(
        &sessions,
        session.clone(),
        descriptor,
        lease.clone(),
        SessionKind::Gpio,
    );
    Ok(envelope::Payload::OpenGpioResponse(
        open_gpio_response_to_proto(&session, &lease),
    ))
}

async fn transfer(
    runtime: HalRuntime,
    request: v1::UsbTransferRequest,
    limits: UsbGpioDispatchLimits,
    sessions: UsbGpioSessions,
) -> HalResult<envelope::Payload> {
    let write_len = request.data.len();
    let read_len = usize::try_from(request.max_bytes).unwrap_or(usize::MAX);
    if write_len > limits.max_write_bytes || read_len > limits.max_read_bytes {
        return Err(invalid_message(
            "USB transfer exceeds negotiated payload limits",
        ));
    }
    let (session, lease, transfer, timeout) = usb_transfer_request_from_proto(request)?;
    let record = validate(
        &sessions,
        &session,
        &lease,
        SessionKind::Usb,
        "usb.transfer",
        false,
    )?;
    let (capability, name) = transfer_capability(&transfer);
    require(&record.capabilities, &capability, name, &record.resource_id)?;
    let data = runtime
        .usb_transfer(session, &lease, transfer, timeout)
        .await?;
    if data.len() > limits.max_read_bytes || response_bound(data.len()) > limits.max_frame_bytes {
        return Err(invalid_message(
            "USB transfer response exceeds negotiated limits",
        ));
    }
    Ok(envelope::Payload::UsbTransferResponse(
        usb_transfer_response_to_proto(data),
    ))
}

async fn read_gpio(
    runtime: HalRuntime,
    request: v1::GpioReadRequest,
    sessions: UsbGpioSessions,
) -> HalResult<envelope::Payload> {
    let (session, lease) = gpio_read_request_from_proto(request)?;
    let record = validate(
        &sessions,
        &session,
        &lease,
        SessionKind::Gpio,
        "gpio.read",
        false,
    )?;
    require(
        &record.capabilities,
        &gpio_lines_capability(),
        GPIO_LINES_CAPABILITY,
        &record.resource_id,
    )?;
    let values = runtime.gpio_read(session, &lease).await?;
    if values.is_empty() || values.len() > MAX_GPIO_EVENTS {
        return Err(invalid_message("GPIO read response violates public bounds"));
    }
    Ok(envelope::Payload::GpioReadResponse(
        gpio_read_response_to_proto(&values),
    ))
}

async fn write_gpio(
    runtime: HalRuntime,
    request: v1::GpioWriteRequest,
    limits: UsbGpioDispatchLimits,
    sessions: UsbGpioSessions,
) -> HalResult<envelope::Payload> {
    if request.values.len() > limits.max_write_bytes {
        return Err(invalid_message("GPIO write exceeds negotiated write limit"));
    }
    let (session, lease, values) = gpio_write_request_from_proto(request)?;
    let record = validate(
        &sessions,
        &session,
        &lease,
        SessionKind::Gpio,
        "gpio.write",
        false,
    )?;
    require(
        &record.capabilities,
        &gpio_lines_capability(),
        GPIO_LINES_CAPABILITY,
        &record.resource_id,
    )?;
    runtime.gpio_write(session, &lease, values).await?;
    Ok(envelope::Payload::GpioWriteResponse(v1::Empty {}))
}

async fn next_edge(
    runtime: HalRuntime,
    request: v1::GpioNextEdgeRequest,
    limits: UsbGpioDispatchLimits,
    sessions: UsbGpioSessions,
) -> HalResult<envelope::Payload> {
    let (session, lease, request, timeout) = gpio_next_edge_request_from_proto(request)?;
    let record = validate(
        &sessions,
        &session,
        &lease,
        SessionKind::Gpio,
        "gpio.next_edge",
        false,
    )?;
    require(
        &record.capabilities,
        &gpio_edges_capability(),
        GPIO_EDGES_CAPABILITY,
        &record.resource_id,
    )?;
    if response_bound(24) > limits.max_frame_bytes {
        return Err(invalid_message(
            "GPIO edge response exceeds negotiated frame limit",
        ));
    }
    Ok(envelope::Payload::GpioNextEdgeResponse(
        seeed_hal_protocol::gpio_next_edge_response_to_proto(
            runtime
                .gpio_next_edge(session, &lease, request, timeout)
                .await?,
        ),
    ))
}

async fn close_usb(
    runtime: HalRuntime,
    request: v1::CloseUsbRequest,
    sessions: UsbGpioSessions,
) -> HalResult<envelope::Payload> {
    let (session, lease) = usb_close_request_from_proto(request)?;
    if !validate(
        &sessions,
        &session,
        &lease,
        SessionKind::Usb,
        "usb.close",
        true,
    )?
    .closed
    {
        runtime.close_usb(session.clone(), &lease).await?;
        record_closed(&sessions, &session);
    }
    Ok(envelope::Payload::CloseUsbResponse(v1::Empty {}))
}

async fn close_gpio(
    runtime: HalRuntime,
    request: v1::CloseGpioRequest,
    sessions: UsbGpioSessions,
) -> HalResult<envelope::Payload> {
    let (session, lease) = gpio_close_request_from_proto(request)?;
    if !validate(
        &sessions,
        &session,
        &lease,
        SessionKind::Gpio,
        "gpio.close",
        true,
    )?
    .closed
    {
        runtime.close_gpio(session.clone(), &lease).await?;
        record_closed(&sessions, &session);
    }
    Ok(envelope::Payload::CloseGpioResponse(v1::Empty {}))
}

fn transfer_capability(transfer: &UsbTransfer) -> (CapabilityId, &'static str) {
    match transfer {
        UsbTransfer::ControlOut { .. } | UsbTransfer::ControlIn { .. } => {
            (usb_control_capability(), USB_CONTROL_CAPABILITY)
        }
        UsbTransfer::BulkOut { .. } | UsbTransfer::BulkIn { .. } => {
            (usb_bulk_capability(), USB_BULK_CAPABILITY)
        }
        UsbTransfer::InterruptOut { .. } | UsbTransfer::InterruptIn { .. } => {
            (usb_interrupt_capability(), USB_INTERRUPT_CAPABILITY)
        }
    }
}

fn select<'a>(
    descriptors: &'a [ResourceDescriptor],
    selector: &ResourceSelector,
    operation: &'static str,
) -> HalResult<&'a ResourceDescriptor> {
    let mut matches = descriptors.iter().filter(|descriptor| {
        descriptor.id() == selector.id()
            && descriptor.transport() == selector.transport()
            && descriptor
                .minimum_identity_quality()
                .satisfies(selector.minimum_identity_quality())
    });
    let Some(descriptor) = matches.next() else {
        return Err(session_error(
            "runtime.resource.not_found",
            operation,
            "resource selector did not match",
            None,
        ));
    };
    if matches.next().is_some() {
        return Err(session_error(
            "runtime.resource.ambiguous",
            operation,
            "resource selector was ambiguous",
            Some(descriptor.id()),
        ));
    }
    Ok(descriptor)
}

fn record(
    sessions: &UsbGpioSessions,
    session: SessionId,
    descriptor: &ResourceDescriptor,
    lease: LeaseToken,
    kind: SessionKind,
) {
    sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .sessions
        .insert(
            session,
            SessionRecord {
                resource_id: descriptor.id().clone(),
                capabilities: descriptor.capabilities().clone(),
                lease,
                kind,
                closed: false,
            },
        );
}

struct SessionContext {
    resource_id: ResourceId,
    capabilities: CapabilitySet,
    closed: bool,
}

fn validate(
    sessions: &UsbGpioSessions,
    session: &SessionId,
    supplied: &LeaseToken,
    kind: SessionKind,
    operation: &'static str,
    allow_closed: bool,
) -> HalResult<SessionContext> {
    let sessions = sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(record) = sessions.sessions.get(session) else {
        return Err(session_error(
            "runtime.session.not_found",
            operation,
            "session is not owned by this broker connection",
            None,
        ));
    };
    if record.kind != kind {
        return Err(session_error(
            "runtime.session.not_found",
            operation,
            "session is not this hardware class",
            None,
        ));
    }
    if &record.lease != supplied {
        let name = if supplied.generation() < record.lease.generation() {
            "runtime.lease.stale_generation"
        } else {
            "runtime.lease.invalid_token"
        };
        return Err(session_error(
            name,
            operation,
            "lease token does not match connection-owned session",
            Some(&record.resource_id),
        ));
    }
    if record.closed && !allow_closed {
        return Err(session_error(
            "runtime.session.closed",
            operation,
            "session is closed",
            Some(&record.resource_id),
        ));
    }
    Ok(SessionContext {
        resource_id: record.resource_id.clone(),
        capabilities: record.capabilities.clone(),
        closed: record.closed,
    })
}

fn record_closed(sessions: &UsbGpioSessions, session: &SessionId) {
    let mut sessions = sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(record) = sessions.sessions.get_mut(session) else {
        return;
    };
    if record.closed {
        return;
    }
    record.closed = true;
    sessions.closed_order.push_back(session.clone());
    while sessions.closed_order.len() > CLOSED_SESSION_RETENTION {
        let evicted = sessions
            .closed_order
            .pop_front()
            .expect("closed sessions are nonempty");
        if sessions
            .sessions
            .get(&evicted)
            .is_some_and(|record| record.closed)
        {
            sessions.sessions.remove(&evicted);
        }
    }
}

fn response_bound(data_len: usize) -> usize {
    24_usize.saturating_add(data_len)
}

fn session_error(
    name: &'static str,
    operation: &'static str,
    message: &'static str,
    resource: Option<&ResourceId>,
) -> HalError {
    let category = match name {
        "runtime.session.not_found" | "runtime.resource.not_found" => ErrorCategory::NotFound,
        "runtime.resource.ambiguous"
        | "runtime.lease.stale_generation"
        | "runtime.lease.invalid_token"
        | "runtime.session.closed"
        | "runtime.protocol.capability_unsupported" => ErrorCategory::Conflict,
        _ => ErrorCategory::InvalidArgument,
    };
    let error = HalError::new(name, category, operation, false, message)
        .expect("static USB/GPIO broker error metadata is valid");
    resource.map_or(error.clone(), |resource| {
        error.with_resource_id(resource.clone())
    })
}
