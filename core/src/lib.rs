// Copyright 2017 Amagicom AB.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.


//! A crate for generating transport agnostic, auto serializing, strongly typed JSON-RPC 2.0
//! clients.
//!
//! This crate mainly provides a macro, `jsonrpc_client`. The macro generates structs that can be
//! used for calling JSON-RPC 2.0 APIs. The macro lets you list methods on the struct with
//! arguments and a return type. The macro then generates a struct which will automatically
//! serialize the arguments, send the request and deserialize the response into the target type.
//!
//! # Transports
//!
//! The `jsonrpc-client-core` crate itself and the structs generated by the `jsonrpc_client` macro
//! are transport agnostic. They can use any type implementing the `Transport` trait.
//!
//! The main (and so far only) transport implementation is the Hyper based HTTP implementation
//! in the [`jsonrpc-client-http`](../jsonrpc_client_http/index.html) crate.
//!
//! # Example
//!
//! ```rust,ignore
//! #[macro_use]
//! extern crate jsonrpc_client_core;
//! extern crate jsonrpc_client_http;
//!
//! use jsonrpc_client_http::HttpTransport;
//!
//! jsonrpc_client!(pub struct FizzBuzzClient {
//!     /// Returns the fizz-buzz string for the given number.
//!     pub fn fizz_buzz(&mut self, number: u64) -> RpcRequest<String>;
//! });
//!
//! fn main() {
//!     let transport = HttpTransport::new().standalone().unwrap();
//!     let transport_handle = transport
//!         .handle("http://api.fizzbuzzexample.org/rpc/")
//!         .unwrap();
//!     let mut client = FizzBuzzClient::new(transport_handle);
//!     let result1 = client.fizz_buzz(3).call().unwrap();
//!     let result2 = client.fizz_buzz(4).call().unwrap();
//!     let result3 = client.fizz_buzz(5).call().unwrap();
//!
//!     // Should print "fizz 4 buzz" if the server implemented the service correctly
//!     println!("{} {} {}", result1, result2, result3);
//! }
//! ```
//!

#![deny(missing_docs)]

#[macro_use]
extern crate error_chain;
#[macro_use]
extern crate futures;
extern crate jsonrpc_core;
#[macro_use]
extern crate log;
extern crate serde;
#[cfg_attr(test, macro_use)]
extern crate serde_json;

use futures::future::Future;
use futures::Async;
use jsonrpc_core::types::{Id, MethodCall, Params, Version};
use serde_json::Value as JsonValue;

/// Contains the main macro of this crate, `jsonrpc_client`.
#[macro_use]
mod macros;

/// Module for functions parsing the response to a RPC method call.
mod response;

/// Module containing an example client. To show in the docs what a generated struct look like.
pub mod example;

error_chain! {
    errors {
        /// Error in the underlying transport layer.
        TransportError {
            description("Unable to send the JSON-RPC 2.0 request")
        }
        /// Error while serializing method parameters.
        SerializeError {
            description("Unable to serialize the method parameters")
        }
        /// Error while deserializing or parsing the response data.
        ResponseError(msg: &'static str) {
            description("Unable to deserialize the response into the desired type")
            display("Unable to deserialize the response: {}", msg)
        }
        /// The request was replied to, but with a JSON-RPC 2.0 error.
        JsonRpcError(error: jsonrpc_core::Error) {
            description("Method call returned JSON-RPC 2.0 error")
            display("JSON-RPC 2.0 Error: {} ({})", error.code.description(), error.message)
        }
    }
}


/// A lazy RPC call `Future`. The actual call has not been sent when an instance of this type
/// is returned from a client generated by the macro in this crate. This is a `Future` that, when
/// executed, performs the RPC call.
pub struct RpcRequest<T, F>(::std::result::Result<InnerRpcRequest<T, F>, Option<Error>>);

impl<T, E, F> RpcRequest<T, F>
where
    T: serde::de::DeserializeOwned + Send + 'static,
    E: ::std::error::Error + Send + 'static,
    F: Future<Item = Vec<u8>, Error = E> + Send + 'static,
{
    /// Consume this RPC request and run it synchronously. This blocks until the RPC call is done,
    /// then the result of the call is returned.
    pub fn call(self) -> Result<T> {
        self.wait()
    }
}

impl<T, E, F> Future for RpcRequest<T, F>
where
    T: serde::de::DeserializeOwned + Send + 'static,
    E: ::std::error::Error + Send + 'static,
    F: Future<Item = Vec<u8>, Error = E> + Send + 'static,
{
    type Item = T;
    type Error = Error;

    fn poll(&mut self) -> futures::Poll<Self::Item, Self::Error> {
        match self.0 {
            Ok(ref mut inner) => inner.poll(),
            Err(ref mut error_option) => Err(error_option
                .take()
                .expect("Cannot call RpcRequest poll twice when in error state")),
        }
    }
}

struct InnerRpcRequest<T, F> {
    transport_future: F,
    id: Id,
    _marker: ::std::marker::PhantomData<T>,
}

impl<T, F> InnerRpcRequest<T, F> {
    fn new(transport_future: F, id: Id) -> Self {
        Self {
            transport_future,
            id,
            _marker: ::std::marker::PhantomData,
        }
    }
}

impl<T, E, F> Future for InnerRpcRequest<T, F>
where
    T: serde::de::DeserializeOwned + Send + 'static,
    E: ::std::error::Error + Send + 'static,
    F: Future<Item = Vec<u8>, Error = E> + Send + 'static,
{
    type Item = T;
    type Error = Error;

    fn poll(&mut self) -> futures::Poll<Self::Item, Self::Error> {
        let response_raw = try_ready!(
            self.transport_future
                .poll()
                .chain_err(|| ErrorKind::TransportError)
        );
        trace!(
            "Deserializing {} byte response to request with id {:?}",
            response_raw.len(),
            self.id
        );
        response::parse(&response_raw, &self.id).map(|t| Async::Ready(t))
    }
}


/// Trait for types acting as a transport layer for the JSON-RPC 2.0 clients generated by the
/// `jsonrpc_client` macro.
pub trait Transport {
    /// The future type this transport returns on send operations.
    type Future: Future<Item = Vec<u8>, Error = Self::Error> + Send + 'static;

    /// The type of error that this transport emits if it fails.
    type Error: ::std::error::Error + Send + 'static;

    /// Returns an id that has not yet been used on this transport. Used by the RPC clients
    /// to fill in the "id" field of a request.
    fn get_next_id(&mut self) -> u64;

    /// Sends the given data over the transport and returns a future that will complete with the
    /// response to the request, or the transport specific error if something went wrong.
    fn send(&self, json_data: Vec<u8>) -> Self::Future;
}


/// Prepares a lazy `RpcRequest` with a given transport, method and parameters.
/// The call is not sent to the transport until the returned `RpcRequest` is actually executed,
/// either as a `Future` or by calling `RpcRequest::call()`.
///
/// # Not intended for direct use
/// This is being called from the client structs generated by the `jsonrpc_client` macro. This
/// function is not intended to be used directly, only the generated structs should call this.
pub fn call_method<T, P, R>(
    transport: &mut T,
    method: String,
    params: P,
) -> RpcRequest<R, T::Future>
where
    T: Transport,
    P: serde::Serialize,
    R: serde::de::DeserializeOwned + Send + 'static,
{
    let id = Id::Num(transport.get_next_id());
    trace!("Serializing call to method \"{}\" with id {:?}", method, id);
    let request_serialization_result =
        serialize_request(id.clone(), method, params).chain_err(|| ErrorKind::SerializeError);
    match request_serialization_result {
        Err(e) => RpcRequest(Err(Some(e))),
        Ok(request_raw) => {
            let transport_future = transport.send(request_raw);
            RpcRequest(Ok(InnerRpcRequest::new(transport_future, id)))
        }
    }
}


/// Creates a JSON-RPC 2.0 request to the given method with the given parameters.
fn serialize_request<P>(
    id: Id,
    method: String,
    params: P,
) -> ::std::result::Result<Vec<u8>, serde_json::error::Error>
where
    P: serde::Serialize,
{
    let serialized_params = match serde_json::to_value(params)? {
        JsonValue::Null => None,
        JsonValue::Array(vec) => Some(Params::Array(vec)),
        JsonValue::Object(obj) => Some(Params::Map(obj)),
        value => Some(Params::Array(vec![value])),
    };
    let method_call = MethodCall {
        jsonrpc: Some(Version::V2),
        method,
        params: serialized_params,
        id,
    };
    serde_json::to_vec(&method_call)
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    pub type BoxFuture<T, E> = Box<Future<Item = T, Error = E> + Send>;

    /// A test transport that just echoes back a response containing the entire request as the
    /// result.
    #[derive(Clone)]
    struct EchoTransport;

    impl Transport for EchoTransport {
        type Future = BoxFuture<Vec<u8>, io::Error>;
        type Error = io::Error;

        fn get_next_id(&mut self) -> u64 {
            1
        }

        fn send(&self, json_data: Vec<u8>) -> Self::Future {
            let json = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": serde_json::from_slice::<JsonValue>(&json_data).unwrap(),
            });
            Box::new(futures::future::ok(serde_json::to_vec(&json).unwrap()))
        }
    }

    /// A transport that always returns an "Invalid request" error
    #[derive(Clone)]
    struct InvalidRequestTransport;

    impl Transport for InvalidRequestTransport {
        type Future = BoxFuture<Vec<u8>, io::Error>;
        type Error = io::Error;

        fn get_next_id(&mut self) -> u64 {
            1
        }

        fn send(&self, _json_data: Vec<u8>) -> Self::Future {
            let json = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": {
                    "code": -32600,
                    "message": "This was an invalid request",
                    "data": [1, 2, 3],
                }
            });
            Box::new(futures::future::ok(serde_json::to_vec(&json).unwrap()))
        }
    }

    /// A transport that always returns a future that fails
    #[derive(Clone)]
    struct ErrorTransport;

    impl Transport for ErrorTransport {
        type Future = BoxFuture<Vec<u8>, io::Error>;
        type Error = io::Error;

        fn get_next_id(&mut self) -> u64 {
            1
        }

        fn send(&self, _json_data: Vec<u8>) -> Self::Future {
            Box::new(futures::future::err(io::Error::new(
                io::ErrorKind::Other,
                "Internal transport error",
            )))
        }
    }

    jsonrpc_client!(pub struct TestRpcClient {
        pub fn ping(&mut self, arg0: &str) -> RpcRequest<JsonValue>;
    });

    #[test]
    fn echo() {
        let mut client = TestRpcClient::new(EchoTransport);
        let result = client.ping("Hello").call().unwrap();
        if let JsonValue::Object(map) = result {
            assert_eq!(Some(&JsonValue::from("2.0")), map.get("jsonrpc"));
            assert_eq!(Some(&JsonValue::from(1)), map.get("id"));
            assert_eq!(Some(&JsonValue::from("ping")), map.get("method"));
            assert_eq!(Some(&JsonValue::from(vec!["Hello"])), map.get("params"));
            assert_eq!(4, map.len());
        } else {
            panic!("Invalid response type: {:?}", result);
        }
    }

    #[test]
    fn invalid_request() {
        let mut client = TestRpcClient::new(InvalidRequestTransport);
        let error = client.ping("").call().unwrap_err();
        if let &ErrorKind::JsonRpcError(ref json_error) = error.kind() {
            use jsonrpc_core::ErrorCode;
            assert_eq!(ErrorCode::InvalidRequest, json_error.code);
            assert_eq!("This was an invalid request", json_error.message);
            assert_eq!(Some(json!{[1, 2, 3]}), json_error.data);
        } else {
            panic!("Wrong error kind");
        }
    }

    #[test]
    fn transport_error() {
        let mut client = TestRpcClient::new(ErrorTransport);
        match client.ping("").call().unwrap_err().kind() {
            &ErrorKind::TransportError => (),
            _ => panic!("Wrong error kind"),
        }
    }
}
