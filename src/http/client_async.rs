//! Async HTTP client
//!
//! This provides a set of APIs for making HTTP(S) requests with async.
//!
//! You can find a usage example at
//! [`examples/http_request.rs`](https://github.com/esp-rs/esp-idf-svc/blob/master/examples/http_request.rs).

use core::cell::UnsafeCell;
use core::future::Future;
use core::pin::Pin;
// use futures::future::FutureExt;

extern crate alloc;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::string::ToString;

use futures::future::LocalBoxFuture; // TODO: Requires alloc
use futures::FutureExt;
use futures::TryFutureExt;

use ::log::*;

use embedded_svc::http::asynch::*;
use embedded_svc::http::client::asynch::*;
use embedded_svc::http::status;
use embedded_svc::io::asynch::{Io, Read, Write};

use esp_idf_sys::*;

use uncased::{Uncased, UncasedStr};

use crate::errors::EspIOError;
use crate::handle::RawHandle;
use crate::private::common::Newtype;
use crate::private::cstr::*;
use crate::tls::X509;

use std::{
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "std", derive(Hash))]
pub enum FollowRedirectsPolicy {
    FollowNone,
    FollowGetHead,
    FollowAll,
}

impl Default for FollowRedirectsPolicy {
    fn default() -> Self {
        Self::FollowGetHead
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct Configuration {
    pub buffer_size: Option<usize>,
    pub buffer_size_tx: Option<usize>,
    pub timeout: Option<core::time::Duration>,
    pub follow_redirects_policy: FollowRedirectsPolicy,
    pub client_certificate: Option<X509<'static>>,
    pub private_key: Option<X509<'static>>,

    pub use_global_ca_store: bool,
    #[cfg(not(esp_idf_version = "4.3"))]
    pub crt_bundle_attach: Option<unsafe extern "C" fn(conf: *mut core::ffi::c_void) -> esp_err_t>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum State {
    New,
    Request,
    Response,
}

#[allow(clippy::type_complexity)]
pub struct EspHttpConnection {
    raw_client: esp_http_client_handle_t,
    follow_redirects_policy: FollowRedirectsPolicy,
    event_handler: Box<Option<Box<dyn Fn(&esp_http_client_event_t) -> esp_err_t>>>,
    state: State,
    request_content_len: u64,
    follow_redirects: bool,
    headers: BTreeMap<Uncased<'static>, String>,
    content_len_header: UnsafeCell<Option<Option<String>>>,
}

impl EspHttpConnection {
    pub fn new(configuration: &Configuration) -> Result<Self, EspError> {
        let event_handler = Box::new(None);

        let mut native_config = esp_http_client_config_t {
            // The ESP-IDF HTTP client is really picky on being initialized with a valid URL
            // So we set something here, which will be changed later anyway, in the request() method
            url: b"http://127.0.0.1\0".as_ptr() as *const _,
            event_handler: Some(Self::on_events),
            user_data: &*event_handler as *const _ as *mut core::ffi::c_void,

            use_global_ca_store: configuration.use_global_ca_store,
            #[cfg(not(esp_idf_version = "4.3"))]
            crt_bundle_attach: configuration.crt_bundle_attach,
            is_async: true, // TODO: Test with multiple async requests
            // https://docs.espressif.com/projects/esp-idf/en/latest/esp32/api-reference/protocols/esp_http_client.html#_CPPv423esp_http_client_perform24esp_http_client_handle_t
            ..Default::default()
        };

        if let Some(buffer_size) = configuration.buffer_size {
            native_config.buffer_size = buffer_size as _;
        };

        if let Some(buffer_size_tx) = configuration.buffer_size_tx {
            native_config.buffer_size_tx = buffer_size_tx as _;
        }

        if let Some(timeout) = configuration.timeout {
            native_config.timeout_ms = timeout.as_millis() as _;
        }

        if let (Some(cert), Some(private_key)) =
            (configuration.client_certificate, configuration.private_key)
        {
            native_config.client_cert_pem = cert.as_esp_idf_raw_ptr() as _;
            native_config.client_cert_len = cert.as_esp_idf_raw_len();

            native_config.client_key_pem = private_key.as_esp_idf_raw_ptr() as _;
            native_config.client_key_len = private_key.as_esp_idf_raw_len();
        }

        let raw_client = unsafe { esp_http_client_init(&native_config) };
        if raw_client.is_null() {
            Err(EspError::from_infallible::<ESP_FAIL>())
        } else {
            Ok(Self {
                raw_client,
                follow_redirects_policy: configuration.follow_redirects_policy,
                event_handler,
                state: State::New,
                request_content_len: 0,
                follow_redirects: false,
                headers: BTreeMap::new(),
                content_len_header: UnsafeCell::new(None),
            })
        }
    }

    pub fn status(&self) -> u16 {
        self.assert_response();
        unsafe { esp_http_client_get_status_code(self.raw_client) as _ }
    }

    pub fn status_message(&self) -> Option<&str> {
        self.assert_response();
        None
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.assert_response();

        if name.eq_ignore_ascii_case("Content-Length") {
            if let Some(content_len_opt) =
                unsafe { self.content_len_header.get().as_mut().unwrap() }.as_ref()
            {
                content_len_opt.as_ref().map(|s| s.as_str())
            } else {
                let content_len = unsafe { esp_http_client_get_content_length(self.raw_client) };
                *unsafe { self.content_len_header.get().as_mut().unwrap() } = if content_len >= 0 {
                    Some(Some(content_len.to_string()))
                } else {
                    None
                };

                unsafe { self.content_len_header.get().as_mut().unwrap() }
                    .as_ref()
                    .and_then(|s| s.as_ref().map(|s| s.as_ref()))
            }
        } else {
            self.headers.get(UncasedStr::new(name)).map(|s| s.as_str())
        }
    }

    pub async fn initiate_request<'a>(
        &'a mut self,
        method: Method,
        uri: &'a str,
        headers: &'a [(&'a str, &'a str)],
    ) -> Result<(), EspError> {
        self.assert_initial();

        let c_uri = CString::new(uri).unwrap();

        esp!(unsafe { esp_http_client_set_url(self.raw_client, c_uri.as_ptr() as _) })?;
        esp!(unsafe {
            esp_http_client_set_method(
                self.raw_client,
                Newtype::<(esp_http_client_method_t, ())>::from(method).0 .0,
            )
        })?;

        let mut content_len = None;

        for (name, value) in headers {
            if name.eq_ignore_ascii_case("Content-Length") {
                if let Ok(len) = value.parse::<u64>() {
                    content_len = Some(len);
                }
            }

            let c_name = CString::new(*name).unwrap();

            // TODO: Replace with a proper conversion from UTF8 to ISO-8859-1
            let c_value = CString::new(*value).unwrap();

            esp!(unsafe {
                esp_http_client_set_header(
                    self.raw_client,
                    c_name.as_ptr() as _,
                    c_value.as_ptr() as _,
                )
            })?;
        }

        self.follow_redirects = match self.follow_redirects_policy {
            FollowRedirectsPolicy::FollowAll => true,
            FollowRedirectsPolicy::FollowGetHead => method == Method::Get || method == Method::Head,
            _ => false,
        };

        self.request_content_len = content_len.unwrap_or(0);

        // TODO: Make this async via on_event callback? But how to share with read + write ?
        // TODO: Convert this to future - how do we poll async?

        // This should be waker, ready on event 1
        ClientFuture::new(self).await;
        unsafe {
            esp_http_client_open(self.raw_client, self.request_content_len as _);
        }
        // self.deregister_handler(); // TODO: This will destroy ALL handlers

        // loop {
        //     match esp!(unsafe {
        //         esp_http_client_open(self.raw_client, self.request_content_len as _)
        //     }) {
        //         Err(e) => {
        //             info!("Connection returned error: {:?}", e);
        //             std::thread::sleep(std::time::Duration::from_millis(100));
        //         }
        //         Ok(t) => {
        //             info!("Connection returned ok: {:?}", t);
        //             break;
        //         }
        //     }
        // }

        self.state = State::Request;

        Ok(())
    }

    pub fn is_request_initiated(&self) -> bool {
        self.state == State::Request
    }

    pub async fn initiate_response(&mut self) -> Result<(), EspError> {
        self.assert_request();

        self.fetch_headers().await?;

        self.state = State::Response;

        Ok(())
    }

    pub fn is_response_initiated(&self) -> bool {
        self.state == State::Response
    }

    pub fn split(&mut self) -> (&EspHttpConnection, &mut Self) {
        self.assert_response();

        let headers_ptr: *const EspHttpConnection = self as *const _;

        // TODO - why not return &self.headers here?
        let headers = unsafe { headers_ptr.as_ref().unwrap() };

        (headers, self)
    }

    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, EspError> {
        self.assert_response();

        // TODO: Make this async - event handler?
        Self::check(unsafe {
            // This is a helper API which internally calls esp_http_client_read multiple times till the end of data is reached or till the buffer gets full.
            esp_http_client_read_response(self.raw_client, buf.as_mut_ptr() as _, buf.len() as _)
        })
    }

    pub async fn write(&mut self, buf: &[u8]) -> Result<usize, EspError> {
        self.assert_request();

        // TODO: Make this async - event handler?
        Self::check(unsafe {
            esp_http_client_write(self.raw_client, buf.as_ptr() as _, buf.len() as _)
        })
    }

    pub async fn flush(&mut self) -> Result<(), EspError> {
        self.assert_request();

        Ok(())
    }

    fn check(result: i32) -> Result<usize, EspError> {
        match EspError::from(result) {
            Some(err) if result < 0 => Err(err),
            _ => Ok(result as _),
        }
    }

    // TODO: Can this be used as async event bus?
    extern "C" fn on_events(event: *mut esp_http_client_event_t) -> esp_err_t {
        match unsafe { event.as_mut() } {
            Some(event) => {
                let handler = event.user_data
                    as *const Option<Box<dyn Fn(&esp_http_client_event_t) -> esp_err_t>>;
                if let Some(handler) = unsafe { handler.as_ref() } {
                    if let Some(handler) = handler.as_ref() {
                        return handler(event);
                    }
                }

                ESP_OK as _
            }
            None => ESP_FAIL as _,
        }
    }

    async fn fetch_headers(&mut self) -> Result<(), EspError> {
        self.headers.clear();
        *self.content_len_header.get_mut() = None;

        loop {
            // TODO: Implement a mechanism where the client can declare in which header it is interested
            let headers_ptr = &mut self.headers as *mut BTreeMap<Uncased, String>;

            let handler = move |event: &esp_http_client_event_t| {
                info!("Received header event: {:?}", &event);
                if event.event_id == esp_http_client_event_id_t_HTTP_EVENT_ON_HEADER {
                    unsafe {
                        // TODO: Replace with a proper conversion from ISO-8859-1 to UTF8

                        headers_ptr.as_mut().unwrap().insert(
                            Uncased::from(from_cstr_ptr(event.header_key).to_string()),
                            from_cstr_ptr(event.header_value).to_string(),
                        );
                    }
                }

                ESP_OK as esp_err_t
            };

            self.register_handler(handler);

            // This function need to call after esp_http_client_open, it will read from http stream, process all receive headers.
            // TODO: Convert to async via Callback future? Is there an event for end of HTTP stream?
            let result = unsafe { esp_http_client_fetch_headers(self.raw_client) };

            self.deregister_handler();

            Self::check(result as _)?;

            trace!("Fetched headers: {:?}", self.headers);

            if self.follow_redirects {
                let status = unsafe { esp_http_client_get_status_code(self.raw_client) as u16 };

                if status::REDIRECT.contains(&status) && status != 304 {
                    info!("Got response {}, about to follow redirect", status);

                    let mut len = 0_i32;
                    esp!(unsafe { esp_http_client_flush_response(self.raw_client, &mut len) })?;
                    esp!(unsafe {
                        esp_http_client_set_method(
                            self.raw_client,
                            esp_http_client_method_t_HTTP_METHOD_GET,
                        )
                    })?;
                    esp!(unsafe { esp_http_client_set_redirection(self.raw_client) })?;
                    esp!(unsafe {
                        esp_http_client_open(self.raw_client, self.request_content_len as _)
                    })?;

                    self.headers.clear();

                    continue;
                }
            }

            break;
        }

        Ok(())
    }

    fn register_handler(
        &mut self,
        handler: impl Fn(&esp_http_client_event_t) -> esp_err_t + 'static,
    ) {
        *self.event_handler = Some(Box::new(handler));
    }

    fn deregister_handler(&mut self) {
        *self.event_handler = None;
    }

    fn assert_initial(&self) {
        if self.state != State::New && self.state != State::Response {
            panic!("connection is not in initial phase");
        }
    }

    fn assert_request(&self) {
        if self.state != State::Request {
            panic!("connection is not in request phase");
        }
    }

    fn assert_response(&self) {
        if self.state != State::Response {
            panic!("connection is not in response phase");
        }
    }
}

impl Drop for EspHttpConnection {
    fn drop(&mut self) {
        esp!(unsafe { esp_http_client_cleanup(self.raw_client) })
            .expect("Unable to stop the client cleanly");
    }
}

impl RawHandle for EspHttpConnection {
    type Handle = esp_http_client_handle_t;

    fn handle(&self) -> Self::Handle {
        self.raw_client
    }
}

impl Status for EspHttpConnection {
    fn status(&self) -> u16 {
        EspHttpConnection::status(self)
    }

    fn status_message(&self) -> Option<&str> {
        EspHttpConnection::status_message(self)
    }
}

impl Headers for EspHttpConnection {
    fn header(&self, name: &str) -> Option<&str> {
        EspHttpConnection::header(self, name)
    }
}

impl Io for EspHttpConnection {
    type Error = EspIOError;
}

impl Read for EspHttpConnection {
    type ReadFuture<'a> =  LocalBoxFuture<'a, Result<usize, Self::Error>>
    where
        Self: 'a;

    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> Self::ReadFuture<'a> {
        Box::pin(EspHttpConnection::read(self, buf).map_err(EspIOError))
    }
}

impl Write for EspHttpConnection {
    type WriteFuture<'a> = LocalBoxFuture<'a, Result<usize, Self::Error>>
    where
        Self: 'a;
    type FlushFuture<'a> = LocalBoxFuture<'a, Result<(), Self::Error>>
    where
        Self: 'a;

    fn write<'a>(&'a mut self, buf: &'a [u8]) -> Self::WriteFuture<'a> {
        Box::pin(EspHttpConnection::write(self, buf).map_err(EspIOError))
    }

    fn flush<'a>(&'_ mut self) -> Self::FlushFuture<'_> {
        Box::pin(EspHttpConnection::flush(self).map_err(EspIOError))
    }
}

impl Connection for EspHttpConnection {
    type Headers = Self;

    type Read = Self;

    type RawConnectionError = EspIOError;

    type RawConnection = Self;

    type IntoRequestFuture<'a> =  LocalBoxFuture<'a, Result<(), Self::Error>>
        where
            Self: 'a;

    type IntoResponseFuture<'a>  = LocalBoxFuture<'a, Result<(), Self::Error>>
        where
            Self: 'a;

    fn initiate_request<'a>(
        &'a mut self,
        method: Method,
        uri: &'a str,
        headers: &'a [(&'a str, &'a str)],
    ) -> Self::IntoRequestFuture<'_> {
        Box::pin(
            EspHttpConnection::initiate_request(self, method, uri, headers)
                .map(|r| r.map_err(EspIOError)),
        )
    }

    fn is_request_initiated(&self) -> bool {
        EspHttpConnection::is_request_initiated(self)
    }

    fn initiate_response(&mut self) -> Self::IntoResponseFuture<'_> {
        Box::pin(EspHttpConnection::initiate_response(self).map(|r| r.map_err(EspIOError)))
    }

    fn is_response_initiated(&self) -> bool {
        EspHttpConnection::is_response_initiated(self)
    }

    fn split(&mut self) -> (&Self::Headers, &mut Self::Read) {
        EspHttpConnection::split(self)
    }

    fn raw_connection(&mut self) -> Result<&mut Self::RawConnection, Self::Error> {
        Err(EspError::from_infallible::<ESP_FAIL>().into())
    }
}

pub struct ClientFuture {
    shared_state: Arc<Mutex<SharedState>>,
}

/// Shared state between the future and the waiting thread
struct SharedState {
    completed: bool, // TODO: Add output for error handling
    waker: Option<Waker>,
}

impl Future for ClientFuture {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut shared_state = self.shared_state.lock().unwrap();
        if shared_state.completed {
            Poll::Ready(())
        } else {
            shared_state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}
impl ClientFuture {
    pub fn new(client: &mut EspHttpConnection) -> Self {
        let shared_state = Arc::new(Mutex::new(SharedState {
            completed: false,
            waker: None,
        }));

        // Spawn the new thread
        let thread_shared_state = shared_state.clone();

        let handler = move |event: &esp_http_client_event_t| {
            info!("Received client future event: {:?}", &event);

            let mut inner_shared_state = thread_shared_state.lock().unwrap();
            if event.event_id == 1 {
                inner_shared_state.completed = true;
            }

            if let Some(waker) = inner_shared_state.waker.take() {
                waker.wake()
            }
            ESP_OK as esp_err_t
        };

        client.register_handler(handler); // TODO: This overwrites any handler in the client - how to manage shared client? Event bus?

        // If error then pending, if Ok then ready
        unsafe {
            esp_http_client_open(client.raw_client, client.request_content_len as _);
        }

        ClientFuture { shared_state }
    }
}