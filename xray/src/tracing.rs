use std::any::Any;
use std::borrow::BorrowMut;
use bytes::Bytes;
use http::{HeaderValue, Request, Response as HttpResponse, StatusCode};
use log::{debug, warn};
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::RwLock;
use tracing::Span;
use tracing_core::{Event, Field, Subscriber};
use tracing_core::field::Visit;
use tracing_core::span::{Attributes, Id, Record};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use crate::{Client, Header, Http, Response, SegmentId, Subsegment, TraceId};
use crate::header::SamplingDecision;

fn segment_for_service(service: &str) -> &str {
    match service {
        "dynamodb" => "DynamoDB",
        _ => service,
    }
}

#[derive(Debug)]
pub(crate) struct SpanContext {
    trace_id: TraceId,
    sampling: SamplingDecision,
    // in fact we should only need to handle cases when segment ID is set, but for type
    // compatibility, let's keepit an Option<>
    parent_id: Option<SegmentId>,
    segment: Option<Subsegment>,
}

impl SpanContext {
    fn root(header: Header) -> Self {
        Self {
            trace_id: header.trace_id,
            sampling: header.sampling_decision,
            parent_id: header.parent_id,
            segment: None,
        }
    }

    fn child(parent: &SpanContext) -> Self {
        Self {
            trace_id: parent.trace_id.clone(),
            sampling: parent.sampling,
            parent_id: parent.segment
                .as_ref()
                .map(|segment| segment.id.clone())
                .or(parent.parent_id.clone()),
            segment: None,
        }
    }
}

#[derive(Default)]
pub(crate) struct XRayTracer {
    segments: HashMap<Id, SpanContext>,
    spans: HashMap<Id, Id>,
}

impl XRayTracer {
    pub fn start_root(&mut self, id: &Id, header: Header) {
        debug!("Starting root context {} for {:?}.", header, id);

        self.segments.insert(id.clone(), SpanContext::root(header));
        self.spans.insert(id.clone(), id.clone());
    }

    pub fn start_child(&mut self, id: &Id, parent_id: &Id) {
        if let Some(parent) = self.get_span_context(&parent_id) {
            debug!("Starting child context {:?} for {:?}.", parent, id);

            let mut current = SpanContext::child(parent);
            current.segment = Some(self.start_segment(&current));

            self.segments.insert(id.clone(), current);
            self.spans.insert(id.clone(), id.clone());
        }
    }

    pub fn continue_segment(&mut self, id: &Id, parent: &Id) {
        if let Some(span_id) = self.spans.get(&parent) {
            self.spans.insert(id.clone(), span_id.clone());
        }
    }

    pub fn end_segment(&mut self, id: &Id) -> Option<Subsegment> {
        self.spans.remove(id);
        // we need to check if there is any segment - might be root of span, we need to close it
        self.segments.remove(id)
            .and_then(|context| context.segment)
    }

    fn start_segment(&self, parent: &SpanContext) -> Subsegment {
        // name will be populated via `on_record()`
        let mut current = Subsegment::begin(
            "AWS",
            SegmentId::new(),
            parent.parent_id.clone(),
            parent.trace_id.clone()
        );
        current.namespace = Some("aws".into());
        current.aws = Some(Default::default());
        current.http = Some(Http::default());
        current
    }

    pub fn get_span_context(&self, span_id: &Id) -> Option<&SpanContext> {
        self.spans.get(span_id)
            .and_then(|span_id| self.segments.get(span_id))
    }

    pub fn get_span_context_mut(&mut self, span_id: &Id) -> Option<&mut SpanContext> {
        if let Some(parent) = self.spans.get(span_id) {
            return self.segments.get_mut(parent);
        }

        None
    }

    fn visitor_for<'a, 'b>(&'a mut self, id: &'b Id) -> XRayVisitor<'a, 'b> {
        XRayVisitor {
            tracer: self,
            id,
        }
    }
}

#[derive(Default)]
pub struct XRaySubscriber {
    client: Client,
    tracer: RwLock<XRayTracer>,
}

impl XRaySubscriber {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_client(client: Client) -> Self {
        Self {
            client,
            ..Self::default()
        }
    }

    fn send_segment(&self, segment: &mut Subsegment) {
        if let Err(e) = self.client.send(&(segment.end())) {
            warn!("Could not send X-Ray trace segment: {:?}", e)
        }
    }

    pub fn instrument_request<BodyType>(&self, request: &mut Request<BodyType>) {
        let tracer = self.tracer.read().unwrap();

        if let Some(header) = Span::current().id()
            .and_then(|span_id| tracer.get_span_context(&span_id))
            .and_then(|context| {
                let mut header = Header::new(context.trace_id.clone());

                HeaderValue::from_str(
                    if let Some(current) = &context.segment {
                        header.with_parent_id(current.id.clone())
                    } else if let Some(parent) = &context.parent_id {
                        header.with_parent_id(parent.clone())
                    } else {
                        &mut header
                    }
                        .with_sampling_decision(context.sampling)
                        .to_string()
                        .as_str()
                ).ok()
            })
        {
            request.headers_mut().insert(Header::NAME, header);
        }
    }
}

impl<S: Subscriber + for<'span> LookupSpan<'span>> Layer<S> for XRaySubscriber {
    fn on_new_span(&self, attrs: &Attributes, id: &Id, ctx: Context<S>) {
        match attrs.metadata().name() {
            // start of Lambda invocation
            "Lambda runtime invoke" => {
                attrs.record(self.tracer.write().unwrap().visitor_for(id).borrow_mut());
            }
            // AWS API call
            "send_operation" => {
                if let Some(parent) = ctx.span(id)
                    .and_then(|span| span.parent())
                    .map(|span| span.id())
                {
                    self.tracer.write().unwrap().start_child(id, &parent);
                }
            }
            // track unknown spans
            _ => {
                // keep span ID mapping for tracing segment frames
                if let Some(parent) = ctx.span(id)
                    .and_then(|span| span.parent())
                    .map(|span| span.id())
                {
                    self.tracer.write().unwrap().continue_segment(id, &parent);
                }
            }
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        let mut tracer = self.tracer.write().unwrap();

        if let Some(segment) = tracer.get_span_context_mut(id)
            .and_then(|context| context.segment.as_mut())
        {
            values.record(segment);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut tracer = self.tracer.write().unwrap();

        if let Some(segment) = ctx.event_span(event)
            .map(|span| span.id())
            .and_then(|span_id| tracer.get_span_context_mut(&span_id))
            .and_then(|context| context.segment.as_mut())
        {
            event.record(segment);
        }
    }

    fn on_close(&self, id: Id, _ctx: Context<S>) {
        // we need to check if there is any segment - might be root
        if let Some(mut segment) = self.tracer.write().unwrap().end_segment(&id) {
            debug!("Sending segment for span {:?}.", id);
            self.send_segment(&mut segment);
        }
    }
}

struct XRayVisitor<'a, 'b> {
    tracer: &'a mut XRayTracer,
    id: &'b Id
}

impl <'a, 'b> Visit for XRayVisitor<'a, 'b> {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "xrayTraceId" => match value.parse::<Header>() {
                Ok(header) => self.tracer.start_root(self.id, header),
                Err(e) => warn!("Could not decode X-Amzn-Trace-ID from {}: {:?}", value, e),
            },
            _ => ()
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn Debug) {
        // noop
    }
}

impl Subsegment {
    fn visit_response(&mut self, http_response: &HttpResponse<Bytes>) {
        let status = http_response.status().as_u16();

        self.error = http_response.status().is_client_error();
        self.fault = http_response.status().is_server_error();
        self.throttled = http_response.status() == StatusCode::TOO_MANY_REQUESTS;

        if let Some(http) = self.http.as_mut() {
            http.response = Some(Response {
                status: Some(status),
                content_length: http_response.headers()
                    .get("content-length")
                    .and_then(|header| header.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
            });
        }
    }
}

impl Visit for Subsegment {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "operation" => if let Some(aws) = self.aws.as_mut() {
                aws.operation = Some(value.into());
            },
            "service" => self.name = segment_for_service(value).into(),
            _ => ()
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        match field.name() {
            "http_response" => {
                // holy crap, this is veyr unsafe!
                self.visit_response(
                    unsafe { &*(value as *const _ as *const &dyn Any as *const &HttpResponse<Bytes>) }
                );
            }
            _ => ()
        }
    }
}
