//! Handles endpoints related to output of payload deltas.

use std::convert::Infallible;
use std::str::FromStr;
use std::sync::Arc;
use futures::stream;
use hyper::{Body, Method, Request};
use rpki::rtr::Serial;
use rpki::rtr::payload::{Action, PayloadRef};
use rpki::rtr::server::PayloadDiff;
use crate::payload::{
    DeltaArcIter, PayloadDelta, PayloadSnapshot, SharedHistory, SnapshotArcIter
};
use crate::utils::fmt::WriteOrPanic;
use super::response::{ContentType, Response, ResponseBuilder};

//------------ handle_get ----------------------------------------------------

pub fn handle_get_or_head(
    req: &Request<Body>,
    history: &SharedHistory,
) -> Option<Response> {
    if req.uri().path() != "/json-delta" {
        return None
    }
    let history = history.read();

    if !history.is_active() {
        return Some(Response::initial_validation())
    }

    let version = match version_from_query(req.uri().query()) {
        Ok(version) => version,
        Err(response) => return Some(response)
    };

    if *req.method() == Method::HEAD {
        return Some(
            ResponseBuilder::ok().content_type(ContentType::JSON).empty()
        )
    }

    if let Some((session, serial)) = version {
        if session == history.session() {
            if let Some(delta) = history.delta_since(serial) {
                return Some(handle_delta(
                    session, serial, history.serial(), delta
                ))
            }
        }
    }

    let snapshot = match history.current() {
        Some(snapshot) => snapshot,
        None => return Some(Response::initial_validation()),
    };
    Some(handle_reset(history.session(), history.serial(), snapshot))
}

fn version_from_query(
    query: Option<&str>
) -> Result<Option<(u64, Serial)>, Response> {
    let query = match query {
        Some(query) => query,
        None => return Ok(None)
    };
    let mut session = None;
    let mut serial = None;

    for (key, value) in form_urlencoded::parse(query.as_ref()) {
        if key == "session" {
            if session.is_some() {
                return Err(Response::bad_request());
            }
            session = Some(u64::from_str(&value).map_err(|_| {
                Response::bad_request()
            })?);
        }
        else if key == "serial" {
            if serial.is_some() {
                return Err(Response::bad_request());
            }
            serial = Some(Serial::from_str(&value).map_err(|_| {
                Response::bad_request()
            })?);
        }
        else {
            return Err(Response::bad_request());
        }
    }
    match (session, serial) {
        (Some(session), Some(serial)) => Ok(Some((session, serial))),
        (None, None) => Ok(None),
        _ => Err(Response::bad_request())
    }
}

fn handle_delta(
    session: u64, from_serial: Serial, to_serial: Serial,
    delta: Arc<PayloadDelta>
) -> Response {
    ResponseBuilder::ok().content_type(ContentType::JSON)
    .body(Body::wrap_stream(stream::iter(
        DeltaStream::new(session, from_serial, to_serial, delta)
        .map(Result::<_, Infallible>::Ok)
    )))
}

fn handle_reset(
    session: u64, to_serial: Serial, snapshot: Arc<PayloadSnapshot>
) -> Response {
    ResponseBuilder::ok().content_type(ContentType::JSON)
    .body(Body::wrap_stream(stream::iter(
        SnapshotStream::new(session, to_serial, snapshot)
        .map(Result::<_, Infallible>::Ok)
    )))
}


//------------ DeltaStream ---------------------------------------------------

/// An iterator as the foundation for streaming a delta.
///
/// The iterator produces segments of roughly 64k size. This can be converted
/// into a async stream and then used with Tokio’s `Body::wrap_stream`.
struct DeltaStream {
    /// The header of the output.
    ///
    /// This is set to some when a new value is created and then taken out on
    /// the very first iteration.
    header: Option<Vec<u8>>,

    /// The iterator for announced items.
    ///
    /// This is a regular delta iterator, we just have to skip over withdraw
    /// items.
    ///
    /// If this is `None`, we are done with the announcements and need to do
    /// the withdrawals.
    announce: Option<DeltaArcIter>,

    /// The iterator for withdrawn items.
    ///
    /// This is a regular delta iterator, we just have to skip over announced
    /// items.
    ///
    /// If this is `None`, there is nothing left to do.
    withdraw: Option<DeltaArcIter>,

    /// Is the next appended item the first item in a list?
    first: bool,
}

impl DeltaStream {
    /// Creates a new delta stream.
    fn new(
        session: u64, from_serial: Serial, to_serial: Serial,
        delta: Arc<PayloadDelta>
    ) -> Self {
        let mut vec = Vec::new();
        Self::append_header(&mut vec, session, from_serial, to_serial);
        DeltaStream {
            header: Some(vec),
            announce: Some(delta.clone().arc_iter()),
            withdraw: Some(delta.arc_iter()),
            first: true,
        }
    }

    /// Appends the delta header to the provided vec.
    fn append_header(
        vec: &mut Vec<u8>,
        session: u64, from_serial: Serial, to_serial: Serial,
    ) {
        write!(vec, "\
            {{\
            \n  \"reset\": false,\
            \n  \"session\": \"{}\",\
            \n  \"serial\": {},\
            \n  \"fromSerial\": {},\
            \n  \"announced\": [",
            session, to_serial, from_serial
        )
    }

    /// Appends the separator between announced and withdrawn to the vec.
    fn append_separator(
        vec: &mut Vec<u8>,
    ) {
        write!(vec, "\
            \n  ],\
            \n  \"withdrawn\": [",
        )
    }

    /// Appends an origin to the vec.
    fn append_payload(
        vec: &mut Vec<u8>,
        payload: PayloadRef,
        first: bool
    ) {
        if !first {
            vec.push(b',')
        }
        match payload {
            PayloadRef::Origin(origin) => {
                write!(vec, "\
                    \n    {{\
                    \n        \"type\": \"routeOrigin\",\
                    \n        \"asn\": \"{}\",\
                    \n        \"prefix\": \"{}/{}\",\
                    \n        \"maxLength\": {}\
                    \n    }}",
                    origin.asn,
                    origin.prefix.addr(), origin.prefix.prefix_len(),
                    origin.prefix.resolved_max_len()
                )
            },
            PayloadRef::RouterKey(key) => {
                write!(vec, "\
                    \n    {{\
                    \n        \"type\": \"routerKey\",\
                    \n        \"keyIdentifier\": \"{}\",\
                    \n        \"asn\": \"{}\",\
                    \n        \"keyInfo\": \"{}\"
                    \n    }}",
                    key.key_identifier,
                    key.asn,
                    key.key_info,
                )
            }
            PayloadRef::Aspa(aspa) => {
                write!(vec, "\
                    \n  {{\
                    \n      \"type\": \"aspa\",
                    \n      \"customerAsn\": \"{}\",\
                    \n      \"afi\": \"{}\",\
                    \n      \"providerAsns\": [",
                    aspa.customer,
                    if aspa.afi.is_ipv4() { "ipv4" } else { "ipv6" },
                );
                let mut first = true;
                for asn in aspa.providers.iter() {
                    if first {
                        write!(vec, "\"{}\"", asn);
                        first = false
                    }
                    else {
                        write!(vec, ", \"{}\"", asn);
                    }
                }
                write!(vec, "]\n\n    }}");
            }
        }
    }

    /// Appends the footer to the vec.
    fn append_footer(vec: &mut Vec<u8>) {
        vec.extend_from_slice(b"\n  ]\n}\n");
    }
}

impl Iterator for DeltaStream {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        #[allow(clippy::question_mark)]
        if self.withdraw.is_none() {
            return None
        }
        let mut vec = self.header.take().unwrap_or_default();
        loop {
            if vec.len() > 64000 {
                return Some(vec)
            }
            if self.next_announce(&mut vec) {
                continue;
            }
            if !self.next_withdraw(&mut vec) {
                return Some(vec)
            }
        }
    }
}

impl DeltaStream {
    /// Appends the next announcement to `vec`.
    ///
    /// Returns whether the method should be called again.
    fn next_announce(&mut self, vec: &mut Vec<u8>) -> bool {
        if let Some(announce) = self.announce.as_mut() {
            while let Some((payload, action)) = announce.next() {
                if matches!(action, Action::Announce) {
                    Self::append_payload(vec, payload, self.first);
                    self.first = false;
                    return true
                }
            }
        }
        else {
            return false;
        }
        Self::append_separator(vec);
        self.announce = None;
        self.first = true;

        // Request to be called again. This only means that if we crossed
        // the 64k boundary, we won’t add the first withdrawal just yet.
        true
    }

    /// Appends the next withdrawal to `vec`.
    ///
    /// Returns whether the method should be called again.
    fn next_withdraw(&mut self, vec: &mut Vec<u8>) -> bool {
        if let Some(withdraw) = self.withdraw.as_mut() {
            while let Some((payload, action)) = withdraw.next() {
                if matches!(action, Action::Withdraw) {
                    Self::append_payload(vec, payload, self.first);
                    self.first = false;
                    return true
                }
            }
        }
        else {
            return false;
        }
        Self::append_footer(vec);
        self.withdraw = None;
        false
    }
}


//------------ SnapshotStream ------------------------------------------------

/// An iterator as the foundation for streaming a snapshot.
///
/// This fairly similar to [`DeltaStream`] only simpler.
struct SnapshotStream {
    /// The header of the output.
    ///
    /// This is set to some when a new value is created and then taken out on
    /// the very first iteration.
    header: Option<Vec<u8>>,

    /// An iterator over the snapshot we work on.
    ///
    /// This is set to `None` to fuse the iterator.
    iter: Option<SnapshotArcIter>,
}

impl SnapshotStream {
    /// Creates a new snapshot stream.
    fn new(
        session: u64, to_serial: Serial, snapshot: Arc<PayloadSnapshot>
    ) -> Self {
        let mut vec = Vec::new();
        Self::append_header(&mut vec, session, to_serial);
        SnapshotStream {
            header: Some(vec),
            iter: Some(snapshot.arc_iter()),
        }
    }

    /// Appends the snapshot header to the vec.
    fn append_header(
        vec: &mut Vec<u8>,
        session: u64, to_serial: Serial,
    ) {
        write!(vec, "\
            {{\
            \n  \"reset\": true,\
            \n  \"session\": \"{}\",\
            \n  \"serial\": {},\
            \n  \"announced\": [",
            session, to_serial,
        )
    }
}

impl Iterator for SnapshotStream {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        use rpki::rtr::server::PayloadSet;

        let iter = self.iter.as_mut()?;
        let mut first = self.header.is_some();
        let mut vec = self.header.take().unwrap_or_default();
        loop {
            if vec.len() > 64000 {
                return Some(vec)
            }
            match iter.next() {
                Some(payload) => {
                    DeltaStream::append_payload(
                        &mut vec, payload, first,
                    );
                }
                None => {
                    break
                }
            }
            first = false;
        }

        self.iter = None;
        DeltaStream::append_footer(&mut vec);
        Some(vec)
    }
}

