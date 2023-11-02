use std::any::type_name;
use std::collections::HashMap;
use std::fmt::Debug;

use access_json::JSONQuery;
use http::header::CONTENT_LENGTH;
use http::header::USER_AGENT;
use opentelemetry_api::baggage::BaggageExt;
use opentelemetry_api::Key;
use opentelemetry_semantic_conventions::trace::HTTP_REQUEST_BODY_SIZE;
use opentelemetry_semantic_conventions::trace::HTTP_RESPONSE_BODY_SIZE;
use opentelemetry_semantic_conventions::trace::HTTP_RESPONSE_STATUS_CODE;
use opentelemetry_semantic_conventions::trace::HTTP_ROUTE;
use opentelemetry_semantic_conventions::trace::NETWORK_PROTOCOL_NAME;
use opentelemetry_semantic_conventions::trace::NETWORK_PROTOCOL_VERSION;
use opentelemetry_semantic_conventions::trace::NETWORK_TRANSPORT;
use opentelemetry_semantic_conventions::trace::SERVER_ADDRESS;
use opentelemetry_semantic_conventions::trace::SERVER_PORT;
use opentelemetry_semantic_conventions::trace::URL_PATH;
use opentelemetry_semantic_conventions::trace::URL_QUERY;
use opentelemetry_semantic_conventions::trace::URL_SCHEME;
use opentelemetry_semantic_conventions::trace::USER_AGENT_ORIGINAL;
use schemars::gen::SchemaGenerator;
use schemars::schema::Schema;
use schemars::JsonSchema;
use serde::de::Error;
use serde::de::MapAccess;
use serde::de::Visitor;
use serde::Deserialize;
use serde::Deserializer;
#[cfg(test)]
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use serde_json_bytes::ByteString;
use tower::BoxError;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::context::OPERATION_KIND;
use crate::context::OPERATION_NAME;
use crate::plugin::serde::deserialize_json_query;
use crate::plugins::telemetry::config::AttributeValue;
use crate::services::router;
use crate::services::subgraph;
use crate::services::supergraph;
use crate::tracer::TraceId;

/// This struct can be used as an attributes container, it has a custom JsonSchema implementation that will merge the schemas of the attributes and custom fields.
#[allow(dead_code)]
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(Serialize))]
pub(crate) struct Extendable<Att, Ext>
where
    Att: Default,
{
    pub(crate) attributes: Att,
    pub(crate) custom: HashMap<String, Ext>,
}

impl Extendable<(), ()> {
    pub(crate) fn empty<A, E>() -> Extendable<A, E>
    where
        A: Default,
    {
        Default::default()
    }
}

/// Custom Deserializer for attributes that will deserializse into a custom field if possible, but otherwise into one of the pre-defined attributes.
impl<'de, Att, Ext> Deserialize<'de> for Extendable<Att, Ext>
where
    Att: Default + Deserialize<'de> + Debug + Sized,
    Ext: Deserialize<'de> + Debug + Sized,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ExtendableVisitor<Att, Ext> {
            _phantom: std::marker::PhantomData<(Att, Ext)>,
        }
        impl<'de, Att, Ext> Visitor<'de> for ExtendableVisitor<Att, Ext>
        where
            Att: Default + Deserialize<'de> + Debug,
            Ext: Deserialize<'de> + Debug,
        {
            type Value = Extendable<Att, Ext>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(formatter, "a map structure")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut attributes: Map<String, Value> = Map::new();
                let mut custom: HashMap<String, Ext> = HashMap::new();
                while let Some(key) = map.next_key()? {
                    let value: Value = map.next_value()?;
                    match Ext::deserialize(value.clone()) {
                        Ok(value) => {
                            custom.insert(key, value);
                        }
                        Err(_err) => {
                            // We didn't manage to deserialize as a custom attribute, so stash the value and we'll try again later
                            attributes.insert(key, value);
                        }
                    }
                }

                let attributes =
                    Att::deserialize(Value::Object(attributes)).map_err(A::Error::custom)?;

                Ok(Extendable { attributes, custom })
            }
        }

        deserializer.deserialize_map(ExtendableVisitor::<Att, Ext> {
            _phantom: Default::default(),
        })
    }
}

impl<A, E> JsonSchema for Extendable<A, E>
where
    A: Default + JsonSchema,
    E: JsonSchema,
{
    fn schema_name() -> String {
        format!(
            "extendable_attribute_{}_{}",
            type_name::<A>(),
            type_name::<E>()
        )
    }

    fn json_schema(gen: &mut SchemaGenerator) -> Schema {
        let mut attributes = gen.subschema_for::<A>();
        let custom = gen.subschema_for::<HashMap<String, E>>();
        if let Schema::Object(schema) = &mut attributes {
            if let Some(object) = &mut schema.object {
                object.additional_properties =
                    custom.into_object().object().additional_properties.clone();
            }
        }

        attributes
    }
}

impl<A, E> Default for Extendable<A, E>
where
    A: Default,
{
    fn default() -> Self {
        Self {
            attributes: Default::default(),
            custom: HashMap::new(),
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Deserialize, JsonSchema, Debug)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub(crate) enum RouterEvent {
    /// When a service request occurs.
    Request,
    /// When a service response occurs.
    Response,
    /// When a service error occurs.
    Error,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema, Clone, Debug, Default)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub(crate) enum DefaultAttributeRequirementLevel {
    /// No default attributes set on spans, you have to set it one by one in the configuration to enable some attributes
    None,
    /// Attributes that are marked as required in otel semantic conventions and apollo documentation will be included (default)
    #[default]
    Required,
    /// Attributes that are marked as required or recommended in otel semantic conventions and apollo documentation will be included
    Recommended,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema, Clone, Debug)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub(crate) enum TraceIdFormat {
    /// Open Telemetry trace ID, a hex string.
    OpenTelemetry,
    /// Datadog trace ID, a u64.
    Datadog,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema, Clone, Debug)]
#[serde(deny_unknown_fields, untagged)]
pub(crate) enum RouterCustomAttribute {
    /// A header from the request
    RequestHeader {
        /// The name of the request header.
        request_header: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    /// A header from the response
    ResponseHeader {
        /// The name of the request header.
        response_header: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    /// The trace ID of the request.
    TraceId {
        /// The format of the trace ID.
        trace_id: TraceIdFormat,
    },
    /// A value from context.
    ResponseContext {
        /// The response context key.
        response_context: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    /// A value from baggage.
    Baggage {
        /// The name of the baggage item.
        baggage: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    /// A value from an environment variable.
    Env {
        /// The name of the environment variable
        env: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<String>,
    },
}
#[allow(dead_code)]
#[derive(Deserialize, JsonSchema, Clone, Debug)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub(crate) enum OperationName {
    /// The raw operation name.
    String,
    /// A hash of the operation name.
    Hash,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema, Clone, Debug)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub(crate) enum Query {
    /// The raw query kind.
    String,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema, Clone, Debug)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub(crate) enum OperationKind {
    /// The raw operation kind.
    String,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema, Clone, Debug)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields, untagged)]
pub(crate) enum SupergraphCustomAttribute {
    OperationName {
        /// The operation name from the query.
        operation_name: OperationName,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<String>,
    },
    OperationKind {
        /// The operation kind from the query (query|mutation|subscription).
        operation_kind: OperationKind,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
    },
    Query {
        /// The graphql query.
        query: Query,
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<String>,
    },
    QueryVariable {
        /// The name of a graphql query variable.
        query_variable: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    RequestHeader {
        /// The name of the request header.
        request_header: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    ResponseHeader {
        /// The name of the response header.
        response_header: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    RequestContext {
        /// The request context key.
        request_context: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    ResponseContext {
        /// The response context key.
        response_context: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    Baggage {
        /// The name of the baggage item.
        baggage: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    Env {
        /// The name of the environment variable
        env: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<String>,
    },
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema, Clone, Debug)]
#[serde(deny_unknown_fields, rename_all = "snake_case", untagged)]
pub(crate) enum SubgraphCustomAttribute {
    SubgraphOperationName {
        /// The operation name from the subgraph query.
        subgraph_operation_name: OperationName,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<String>,
    },
    SubgraphOperationKind {
        /// The kind of the subgraph operation (query|mutation|subscription).
        subgraph_operation_kind: OperationKind,
    },
    SubgraphQuery {
        /// The graphql query to the subgraph.
        subgraph_query: Query,
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<String>,
    },
    SubgraphQueryVariable {
        /// The name of a subgraph query variable.
        subgraph_query_variable: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    SubgraphResponseBody {
        /// The subgraph response body json path.
        #[schemars(with = "String")]
        #[serde(deserialize_with = "deserialize_json_query")]
        subgraph_response_body: JSONQuery,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    SubgraphRequestHeader {
        /// The name of the subgraph request header.
        subgraph_request_header: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    SubgraphResponseHeader {
        /// The name of the subgraph response header.
        subgraph_response_header: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },

    SupergraphOperationName {
        /// The supergraph query operation name.
        supergraph_operation_name: OperationName,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<String>,
    },
    SupergraphOperationKind {
        /// The supergraph query operation kind (query|mutation|subscription).
        supergraph_operation_kind: OperationKind,
    },
    SupergraphQueryVariable {
        /// The supergraph query variable name.
        supergraph_query_variable: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    SupergraphRequestHeader {
        /// The supergraph request header name.
        supergraph_request_header: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    SupergraphResponseHeader {
        /// The supergraph response header name.
        supergraph_response_header: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    RequestContext {
        /// The request context key.
        request_context: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    ResponseContext {
        /// The response context key.
        response_context: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    Baggage {
        /// The name of the baggage item.
        baggage: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<AttributeValue>,
    },
    Env {
        /// The name of the environment variable
        env: String,
        #[serde(skip)]
        /// Optional redaction pattern.
        redact: Option<String>,
        /// Optional default value.
        default: Option<String>,
    },
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema, Clone, Default, Debug)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct RouterAttributes {
    /// Http attributes from Open Telemetry semantic conventions.
    #[serde(flatten)]
    common: HttpCommonAttributes,
    /// Http server attributes from Open Telemetry semantic conventions.
    #[serde(flatten)]
    server: HttpServerAttributes,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema, Clone, Default, Debug)]
#[cfg_attr(test, derive(Serialize))]
#[serde(deny_unknown_fields, default)]
pub(crate) struct SupergraphAttributes {
    /// The GraphQL document being executed.
    /// Examples:
    /// * query findBookById { bookById(id: ?) { name } }
    /// Requirement level: Recommended
    #[serde(rename = "graphql.document")]
    pub(crate) graphql_document: Option<bool>,
    /// The name of the operation being executed.
    /// Examples:
    /// * findBookById
    /// Requirement level: Recommended
    #[serde(rename = "graphql.operation.name")]
    pub(crate) graphql_operation_name: Option<bool>,
    /// The type of the operation being executed.
    /// Examples:
    /// * query
    /// * subscription
    /// * mutation
    /// Requirement level: Recommended
    #[serde(rename = "graphql.operation.type")]
    pub(crate) graphql_operation_type: Option<bool>,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema, Clone, Default, Debug)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct SubgraphAttributes {
    /// The name of the subgraph
    /// Examples:
    /// * products
    /// Requirement level: Required
    #[serde(rename = "graphql.federation.subgraph.name")]
    pub(crate) graphql_federation_subgraph_name: Option<bool>,
    /// The GraphQL document being executed.
    /// Examples:
    /// * query findBookById { bookById(id: ?) { name } }
    /// Requirement level: Recommended
    #[serde(rename = "graphql.document")]
    pub(crate) graphql_document: Option<bool>,
    /// The name of the operation being executed.
    /// Examples:
    /// * findBookById
    /// Requirement level: Recommended
    #[serde(rename = "graphql.operation.name")]
    pub(crate) graphql_operation_name: Option<bool>,
    /// The type of the operation being executed.
    /// Examples:
    /// * query
    /// * subscription
    /// * mutation
    /// Requirement level: Recommended
    #[serde(rename = "graphql.operation.type")]
    pub(crate) graphql_operation_type: Option<bool>,
}

/// Common attributes for http server and client.
/// See https://opentelemetry.io/docs/specs/semconv/http/http-spans/#common-attributes
#[allow(dead_code)]
#[derive(Deserialize, JsonSchema, Clone, Default, Debug)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct HttpCommonAttributes {
    /// Describes a class of error the operation ended with.
    /// Examples:
    /// * timeout
    /// * name_resolution_error
    /// * 500
    /// Requirement level: Conditionally Required: If request has ended with an error.
    #[serde(rename = "error.type")]
    pub(crate) error_type: Option<bool>,

    /// The size of the request payload body in bytes. This is the number of bytes transferred excluding headers and is often, but not always, present as the Content-Length header. For requests using transport encoding, this should be the compressed size.
    /// Examples:
    /// * 3495
    /// Requirement level: Recommended
    #[serde(rename = "http.request.body.size")]
    pub(crate) http_request_body_size: Option<bool>,

    /// HTTP request method.
    /// Examples:
    /// * GET
    /// * POST
    /// * HEAD
    /// Requirement level: Required
    #[serde(rename = "http.request.method")]
    pub(crate) http_request_method: Option<bool>,

    /// Original HTTP method sent by the client in the request line.
    /// Examples:
    /// * GeT
    /// * ACL
    /// * foo
    /// Requirement level: Conditionally Required (If and only if it’s different than http.request.method)
    #[serde(rename = "http.request.method.original")]
    pub(crate) http_request_method_original: Option<bool>,

    /// The size of the response payload body in bytes. This is the number of bytes transferred excluding headers and is often, but not always, present as the Content-Length header. For requests using transport encoding, this should be the compressed size.
    /// Examples:
    /// * 3495
    /// Requirement level: Recommended
    #[serde(rename = "http.response.body.size")]
    pub(crate) http_response_body_size: Option<bool>,

    /// HTTP response status code.
    /// Examples:
    /// * 200
    /// Requirement level: Conditionally Required: If and only if one was received/sent.
    #[serde(rename = "http.response.status_code")]
    pub(crate) http_response_status_code: Option<bool>,

    /// OSI application layer or non-OSI equivalent.
    /// Examples:
    /// * http
    /// * spdy
    /// Requirement level: Recommended: if not default (http).
    #[serde(rename = "network.protocol.name")]
    pub(crate) network_protocol_name: Option<bool>,

    /// Version of the protocol specified in network.protocol.name.
    /// Examples:
    /// * 1.0
    /// * 1.1
    /// * 2
    /// * 3
    /// Requirement level: Recommended
    #[serde(rename = "network.protocol.version")]
    pub(crate) network_protocol_version: Option<bool>,

    /// OSI transport layer.
    /// Examples:
    /// * tcp
    /// * udp
    /// Requirement level: Conditionally Required
    #[serde(rename = "network.transport")]
    pub(crate) network_transport: Option<bool>,

    /// OSI network layer or non-OSI equivalent.
    /// Examples:
    /// * ipv4
    /// * ipv6
    /// Requirement level: Recommended
    #[serde(rename = "network.type")]
    pub(crate) network_type: Option<bool>,

    /// Value of the HTTP User-Agent header sent by the client.
    /// Examples:
    /// * CERN-LineMode/2.15
    /// * libwww/2.17b3
    /// Requirement level: Recommended
    #[serde(rename = "user_agent.original")]
    pub(crate) user_agent_original: Option<bool>,
}

/// Attributes for Http servers
/// See https://opentelemetry.io/docs/specs/semconv/http/http-spans/#http-server
#[allow(dead_code)]
#[derive(Deserialize, JsonSchema, Clone, Default, Debug)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct HttpServerAttributes {
    /// Client address - domain name if available without reverse DNS lookup, otherwise IP address or Unix domain socket name.
    /// Examples:
    /// * 83.164.160.102
    /// Requirement level: Recommended
    #[serde(rename = "client.address", skip)]
    client_address: Option<bool>,
    /// The port of the original client behind all proxies, if known (e.g. from Forwarded or a similar header). Otherwise, the immediate client peer port.
    /// Examples:
    /// * 83.164.160.102
    /// Requirement level: Recommended
    #[serde(rename = "client.port", skip)]
    client_port: Option<bool>,
    /// The matched route (path template in the format used by the respective server framework).
    /// Examples:
    /// * 65123
    /// Requirement level: Conditionally Required: If and only if it’s available
    #[serde(rename = "http.route")]
    http_route: Option<bool>,
    /// Local socket address. Useful in case of a multi-IP host.
    /// Examples:
    /// * 10.1.2.80
    /// * /tmp/my.sock
    /// Requirement level: Opt-In
    #[serde(rename = "network.local.address", skip)]
    network_local_address: Option<bool>,
    /// Local socket port. Useful in case of a multi-port host.
    /// Examples:
    /// * 65123
    /// Requirement level: Opt-In
    #[serde(rename = "network.local.port", skip)]
    network_local_port: Option<bool>,
    /// Peer address of the network connection - IP address or Unix domain socket name.
    /// Examples:
    /// * 10.1.2.80
    /// * /tmp/my.sock
    /// Requirement level: Recommended
    #[serde(rename = "network.peer.address", skip)]
    network_peer_address: Option<bool>,
    /// Peer port number of the network connection.
    /// Examples:
    /// * 65123
    /// Requirement level: Recommended
    #[serde(rename = "network.peer.port", skip)]
    network_peer_port: Option<bool>,
    /// Name of the local HTTP server that received the request.
    /// Examples:
    /// * example.com
    /// * 10.1.2.80
    /// * /tmp/my.sock
    /// Requirement level: Recommended
    #[serde(rename = "server.address")]
    server_address: Option<bool>,
    /// Port of the local HTTP server that received the request.
    /// Examples:
    /// * 80
    /// * 8080
    /// * 443
    /// Requirement level: Recommended
    #[serde(rename = "server.port")]
    server_port: Option<bool>,
    /// The URI path component
    /// Examples:
    /// * /search
    /// Requirement level: Required
    #[serde(rename = "url.path")]
    url_path: Option<bool>,
    /// The URI query component
    /// Examples:
    /// * q=OpenTelemetry
    /// Requirement level: Conditionally Required: If and only if one was received/sent.
    #[serde(rename = "url.query")]
    url_query: Option<bool>,

    /// The URI scheme component identifying the used protocol.
    /// Examples:
    /// * http
    /// * https
    /// Requirement level: Required
    #[serde(rename = "url.scheme")]
    url_scheme: Option<bool>,
}

/// Attrubtes for HTTP clients
/// https://opentelemetry.io/docs/specs/semconv/http/http-spans/#http-client
#[allow(dead_code)]
#[derive(Deserialize, JsonSchema, Clone, Default, Debug)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct HttpClientAttributes {
    /// The ordinal number of request resending attempt.
    /// Examples:
    /// *
    /// Requirement level: Recommended: if and only if request was retried.
    #[serde(rename = "http.resend_count")]
    http_resend_count: Option<bool>,

    /// Peer address of the network connection - IP address or Unix domain socket name.
    /// Examples:
    /// * 10.1.2.80
    /// * /tmp/my.sock
    /// Requirement level: Recommended: If different than server.address.
    #[serde(rename = "network.peer.address")]
    network_peer_address: Option<bool>,

    /// Peer port number of the network connection.
    /// Examples:
    /// * 65123
    /// Requirement level: Recommended: If network.peer.address is set.
    #[serde(rename = "network.peer.port")]
    network_peer_port: Option<bool>,

    /// Host identifier of the “URI origin” HTTP request is sent to.
    /// Examples:
    /// * example.com
    /// * 10.1.2.80
    /// * /tmp/my.sock
    /// Requirement level: Required
    #[serde(rename = "server.address")]
    server_address: Option<bool>,

    /// Port identifier of the “URI origin” HTTP request is sent to.
    /// Examples:
    /// * 80
    /// * 8080
    /// * 433
    /// Requirement level: Conditionally Required
    #[serde(rename = "server.port")]
    server_port: Option<bool>,

    /// Absolute URL describing a network resource according to RFC3986
    /// Examples:
    /// * https://www.foo.bar/search?q=OpenTelemetry#SemConv;
    /// * localhost
    /// Requirement level: Required
    #[serde(rename = "url.full")]
    url_full: Option<bool>,
}

pub(crate) trait GetAttributes<Request, Response> {
    fn on_request(&self, request: &Request) -> HashMap<Key, AttributeValue>;
    fn on_response(&self, response: &Response) -> HashMap<Key, AttributeValue>;
    fn on_error(&self, error: &BoxError) -> HashMap<Key, AttributeValue>;
}

pub(crate) trait GetAttribute<Request, Response> {
    fn on_request(&self, request: &Request) -> Option<AttributeValue>;
    fn on_response(&self, response: &Response) -> Option<AttributeValue>;
}

impl<A, E, Request, Response> GetAttributes<Request, Response> for Extendable<A, E>
where
    A: Default + GetAttributes<Request, Response>,
    E: GetAttribute<Request, Response>,
{
    fn on_request(&self, request: &Request) -> HashMap<Key, AttributeValue> {
        let mut attrs = self.attributes.on_request(request);
        let custom_attributes = self.custom.iter().filter_map(|(key, value)| {
            value
                .on_request(request)
                .map(|v| (Key::from(key.clone()), v))
        });
        attrs.extend(custom_attributes);

        attrs
    }

    fn on_response(&self, response: &Response) -> HashMap<Key, AttributeValue> {
        let mut attrs = self.attributes.on_response(response);
        let custom_attributes = self.custom.iter().filter_map(|(key, value)| {
            value
                .on_response(response)
                .map(|v| (Key::from(key.clone()), v))
        });
        attrs.extend(custom_attributes);

        attrs
    }

    fn on_error(&self, error: &BoxError) -> HashMap<Key, AttributeValue> {
        self.attributes.on_error(error)
    }
}

impl GetAttribute<router::Request, router::Response> for RouterCustomAttribute {
    fn on_request(&self, request: &router::Request) -> Option<AttributeValue> {
        match self {
            RouterCustomAttribute::RequestHeader {
                request_header,
                default,
                ..
            } => request
                .router_request
                .headers()
                .get(request_header)
                .and_then(|h| Some(AttributeValue::String(h.to_str().ok()?.to_string())))
                .or_else(|| default.clone()),
            RouterCustomAttribute::Env { env, default, .. } => std::env::var(env)
                .ok()
                .map(AttributeValue::String)
                .or_else(|| default.clone().map(AttributeValue::String)),
            RouterCustomAttribute::TraceId {
                trace_id: trace_id_format,
            } => {
                let trace_id = TraceId::maybe_new()?;
                match trace_id_format {
                    TraceIdFormat::OpenTelemetry => AttributeValue::String(trace_id.to_string()),
                    TraceIdFormat::Datadog => AttributeValue::U128(trace_id.to_u128()),
                }
                .into()
            }
            RouterCustomAttribute::Baggage {
                baggage: baggage_name,
                default,
                ..
            } => {
                let span = Span::current();
                let span_context = span.context();
                // I must clone the key because the otel API is bad
                let baggage = span_context.baggage().get(baggage_name.clone()).cloned();
                match baggage {
                    Some(baggage) => AttributeValue::from(baggage).into(),
                    None => default.clone(),
                }
            }
            // Related to Response
            _ => None,
        }
    }

    fn on_response(&self, response: &router::Response) -> Option<AttributeValue> {
        match self {
            RouterCustomAttribute::ResponseHeader {
                response_header,
                default,
                ..
            } => response
                .response
                .headers()
                .get(response_header)
                .and_then(|h| Some(AttributeValue::String(h.to_str().ok()?.to_string())))
                .or_else(|| default.clone()),
            RouterCustomAttribute::ResponseContext {
                response_context,
                default,
                ..
            } => response
                .context
                .get(response_context)
                .ok()
                .flatten()
                .or_else(|| default.clone()),
            RouterCustomAttribute::Baggage {
                baggage: baggage_name,
                default,
                ..
            } => {
                let span = Span::current();
                let span_context = span.context();
                // I must clone the key because the otel API is bad
                let baggage = span_context.baggage().get(baggage_name.clone()).cloned();
                match baggage {
                    Some(baggage) => AttributeValue::from(baggage).into(),
                    None => default.clone(),
                }
            }
            _ => None,
        }
    }
}

impl GetAttributes<router::Request, router::Response> for RouterAttributes {
    fn on_request(&self, request: &router::Request) -> HashMap<Key, AttributeValue> {
        self.common.on_request(request)
    }

    fn on_response(&self, response: &router::Response) -> HashMap<Key, AttributeValue> {
        self.common.on_response(response)
    }

    fn on_error(&self, error: &BoxError) -> HashMap<Key, AttributeValue> {
        self.common.on_error(error)
    }
}

impl GetAttributes<router::Request, router::Response> for HttpCommonAttributes {
    fn on_request(&self, request: &router::Request) -> HashMap<Key, AttributeValue> {
        let mut attrs = HashMap::new();
        if let Some(true) = &self.http_request_body_size {
            if let Some(content_length) = request
                .router_request
                .headers()
                .get(&CONTENT_LENGTH)
                .and_then(|h| h.to_str().ok())
            {
                attrs.insert(
                    HTTP_REQUEST_BODY_SIZE,
                    AttributeValue::String(content_length.to_string()),
                );
            }
        }
        if let Some(true) = &self.network_protocol_name {
            attrs.insert(
                NETWORK_PROTOCOL_NAME,
                AttributeValue::String("http".to_string()),
            );
        }
        if let Some(true) = &self.network_protocol_version {
            attrs.insert(
                NETWORK_PROTOCOL_VERSION,
                AttributeValue::String(format!("{:?}", request.router_request.version())),
            );
        }
        if let Some(true) = &self.network_transport {
            attrs.insert(NETWORK_TRANSPORT, AttributeValue::String("tcp".to_string()));
        }
        if let Some(true) = &self.user_agent_original {
            if let Some(user_agent) = request
                .router_request
                .headers()
                .get(&USER_AGENT)
                .and_then(|h| h.to_str().ok())
            {
                attrs.insert(
                    USER_AGENT_ORIGINAL,
                    AttributeValue::String(user_agent.to_string()),
                );
            }
        }

        attrs
    }

    fn on_response(&self, response: &router::Response) -> HashMap<Key, AttributeValue> {
        let mut attrs = HashMap::new();
        if let Some(true) = &self.http_response_body_size {
            if let Some(content_length) = response
                .response
                .headers()
                .get(&CONTENT_LENGTH)
                .and_then(|h| h.to_str().ok())
            {
                attrs.insert(
                    HTTP_RESPONSE_BODY_SIZE,
                    AttributeValue::String(content_length.to_string()),
                );
            }
        }
        if let Some(true) = &self.http_response_status_code {
            attrs.insert(
                HTTP_RESPONSE_STATUS_CODE,
                AttributeValue::String(response.response.status().to_string()),
            );
        }
        attrs
    }

    fn on_error(&self, _error: &BoxError) -> HashMap<Key, AttributeValue> {
        let mut attrs = HashMap::new();
        if let Some(true) = &self.error_type {
            attrs.insert(Key::from_static_str("error.type"), AttributeValue::I64(500));
        }

        attrs
    }
}

impl GetAttributes<router::Request, router::Response> for HttpServerAttributes {
    fn on_request(&self, request: &router::Request) -> HashMap<Key, AttributeValue> {
        let mut attrs = HashMap::new();
        if let Some(true) = &self.http_route {
            attrs.insert(
                HTTP_ROUTE,
                AttributeValue::String(request.router_request.uri().to_string()),
            );
        }
        let router_uri = request.router_request.uri();
        if let Some(true) = &self.server_address {
            if let Some(host) = router_uri.host() {
                attrs.insert(SERVER_ADDRESS, AttributeValue::String(host.to_string()));
            }
        }
        if let Some(true) = &self.server_port {
            if let Some(port) = router_uri.port() {
                attrs.insert(SERVER_PORT, AttributeValue::String(port.to_string()));
            }
        }
        if let Some(true) = &self.url_path {
            attrs.insert(
                URL_PATH,
                AttributeValue::String(router_uri.path().to_string()),
            );
        }
        if let Some(true) = &self.url_query {
            if let Some(query) = router_uri.query() {
                attrs.insert(URL_QUERY, AttributeValue::String(query.to_string()));
            }
        }
        if let Some(true) = &self.url_scheme {
            if let Some(scheme) = router_uri.scheme_str() {
                attrs.insert(URL_SCHEME, AttributeValue::String(scheme.to_string()));
            }
        }

        attrs
    }

    fn on_response(&self, _response: &router::Response) -> HashMap<Key, AttributeValue> {
        HashMap::with_capacity(0)
    }

    fn on_error(&self, _error: &BoxError) -> HashMap<Key, AttributeValue> {
        HashMap::with_capacity(0)
    }
}

impl GetAttribute<supergraph::Request, supergraph::Response> for SupergraphCustomAttribute {
    fn on_request(&self, request: &supergraph::Request) -> Option<AttributeValue> {
        match self {
            SupergraphCustomAttribute::OperationName {
                operation_name,
                default,
                ..
            } => {
                let op_name = request.context.get(OPERATION_NAME).ok().flatten();
                match operation_name {
                    OperationName::String => {
                        op_name.or_else(|| default.clone().map(AttributeValue::String))
                    }
                    OperationName::Hash => todo!(),
                }
            }
            SupergraphCustomAttribute::OperationKind { .. } => {
                request.context.get(OPERATION_KIND).ok().flatten()
            }
            SupergraphCustomAttribute::QueryVariable {
                query_variable,
                default,
                ..
            } => request
                .supergraph_request
                .body()
                .variables
                .get(&ByteString::from(query_variable.as_str()))
                .and_then(|v| serde_json::to_string(v).ok())
                .map(AttributeValue::String)
                .or_else(|| default.clone()),
            SupergraphCustomAttribute::RequestHeader {
                request_header,
                default,
                ..
            } => request
                .supergraph_request
                .headers()
                .get(request_header)
                .and_then(|h| Some(AttributeValue::String(h.to_str().ok()?.to_string())))
                .or_else(|| default.clone()),
            SupergraphCustomAttribute::RequestContext {
                request_context,
                default,
                ..
            } => request
                .context
                .get(request_context)
                .ok()
                .flatten()
                .or_else(|| default.clone()),
            SupergraphCustomAttribute::Baggage {
                baggage: baggage_name,
                default,
                ..
            } => {
                let span = Span::current();
                let span_context = span.context();
                // I must clone the key because the otel API is bad
                let baggage = span_context.baggage().get(baggage_name.clone()).cloned();
                match baggage {
                    Some(baggage) => AttributeValue::from(baggage).into(),
                    None => default.clone(),
                }
            }
            SupergraphCustomAttribute::Env { env, default, .. } => std::env::var(env)
                .ok()
                .map(AttributeValue::String)
                .or_else(|| default.clone().map(AttributeValue::String)),
            // For response
            _ => None,
        }
    }

    fn on_response(&self, response: &supergraph::Response) -> Option<AttributeValue> {
        match self {
            SupergraphCustomAttribute::ResponseHeader {
                response_header,
                default,
                ..
            } => response
                .response
                .headers()
                .get(response_header)
                .and_then(|h| Some(AttributeValue::String(h.to_str().ok()?.to_string())))
                .or_else(|| default.clone()),
            SupergraphCustomAttribute::ResponseContext {
                response_context,
                default,
                ..
            } => response
                .context
                .get(response_context)
                .ok()
                .flatten()
                .or_else(|| default.clone()),
            // For request
            _ => None,
        }
    }
}

impl GetAttribute<subgraph::Request, subgraph::Response> for SubgraphCustomAttribute {
    fn on_request(&self, request: &subgraph::Request) -> Option<AttributeValue> {
        match self {
            SubgraphCustomAttribute::SubgraphOperationName {
                subgraph_operation_name,
                default,
                ..
            } => {
                let op_name = request.subgraph_request.body().operation_name.clone();
                match subgraph_operation_name {
                    OperationName::String => op_name
                        .map(AttributeValue::String)
                        .or_else(|| default.clone().map(AttributeValue::String)),
                    OperationName::Hash => todo!(),
                }
            }
            SubgraphCustomAttribute::SupergraphOperationName {
                supergraph_operation_name,
                default,
                ..
            } => {
                let op_name = request.context.get(OPERATION_NAME).ok().flatten();
                match supergraph_operation_name {
                    OperationName::String => {
                        op_name.or_else(|| default.clone().map(AttributeValue::String))
                    }
                    OperationName::Hash => todo!(),
                }
            }
            SubgraphCustomAttribute::SubgraphOperationKind { .. } => AttributeValue::String(
                request
                    .operation_kind
                    .as_apollo_operation_type()
                    .to_string(),
            )
            .into(),
            SubgraphCustomAttribute::SupergraphOperationKind { .. } => {
                request.context.get(OPERATION_KIND).ok().flatten()
            }
            SubgraphCustomAttribute::SubgraphQueryVariable {
                subgraph_query_variable,
                default,
                ..
            } => request
                .subgraph_request
                .body()
                .variables
                .get(&ByteString::from(subgraph_query_variable.as_str()))
                .and_then(|v| serde_json::to_string(v).ok())
                .map(AttributeValue::String)
                .or_else(|| default.clone()),
            SubgraphCustomAttribute::SupergraphQueryVariable {
                supergraph_query_variable,
                default,
                ..
            } => request
                .supergraph_request
                .body()
                .variables
                .get(&ByteString::from(supergraph_query_variable.as_str()))
                .and_then(|v| serde_json::to_string(v).ok())
                .map(AttributeValue::String)
                .or_else(|| default.clone()),
            SubgraphCustomAttribute::SubgraphRequestHeader {
                subgraph_request_header,
                default,
                ..
            } => request
                .subgraph_request
                .headers()
                .get(subgraph_request_header)
                .and_then(|h| Some(AttributeValue::String(h.to_str().ok()?.to_string())))
                .or_else(|| default.clone()),
            SubgraphCustomAttribute::SupergraphRequestHeader {
                supergraph_request_header,
                default,
                ..
            } => request
                .supergraph_request
                .headers()
                .get(supergraph_request_header)
                .and_then(|h| Some(AttributeValue::String(h.to_str().ok()?.to_string())))
                .or_else(|| default.clone()),
            SubgraphCustomAttribute::RequestContext {
                request_context,
                default,
                ..
            } => request
                .context
                .get(request_context)
                .ok()
                .flatten()
                .or_else(|| default.clone()),
            SubgraphCustomAttribute::Baggage {
                baggage: baggage_name,
                default,
                ..
            } => {
                let span = Span::current();
                let span_context = span.context();
                // I must clone the key because the otel API is bad
                let baggage = span_context.baggage().get(baggage_name.clone()).cloned();
                match baggage {
                    Some(baggage) => AttributeValue::from(baggage).into(),
                    None => default.clone(),
                }
            }
            SubgraphCustomAttribute::Env { env, default, .. } => std::env::var(env)
                .ok()
                .map(AttributeValue::String)
                .or_else(|| default.clone().map(AttributeValue::String)),
            // For response
            _ => None,
        }
    }

    fn on_response(&self, response: &subgraph::Response) -> Option<AttributeValue> {
        match self {
            SubgraphCustomAttribute::SubgraphResponseHeader {
                subgraph_response_header,
                default,
                ..
            } => response
                .response
                .headers()
                .get(subgraph_response_header)
                .and_then(|h| Some(AttributeValue::String(h.to_str().ok()?.to_string())))
                .or_else(|| default.clone()),
            SubgraphCustomAttribute::SubgraphResponseBody {
                subgraph_response_body,
                default,
                ..
            } => {
                let output = subgraph_response_body
                    .execute(response.response.body())
                    .ok()
                    .flatten()?;
                AttributeValue::try_from(output)
                    .ok()
                    .or_else(|| default.clone())
            }
            SubgraphCustomAttribute::ResponseContext {
                response_context,
                default,
                ..
            } => response
                .context
                .get(response_context)
                .ok()
                .flatten()
                .or_else(|| default.clone()),
            // For request
            _ => None,
        }
    }
}

pub(crate) trait DefaultForLevel {
    fn defaults_for_level(&mut self, requirement_level: &DefaultAttributeRequirementLevel);
}

impl DefaultForLevel for HttpCommonAttributes {
    fn defaults_for_level(&mut self, requirement_level: &DefaultAttributeRequirementLevel) {
        match requirement_level {
            DefaultAttributeRequirementLevel::Required => {
                if self.error_type.is_none() {
                    self.error_type = Some(true);
                }
                if self.http_request_method.is_none() {
                    self.http_request_method = Some(true);
                }
                if self.http_response_status_code.is_none() {
                    self.http_response_status_code = Some(true);
                }
            }
            DefaultAttributeRequirementLevel::Recommended => {
                // Required
                if self.error_type.is_none() {
                    self.error_type = Some(true);
                }

                if self.http_request_method.is_none() {
                    self.http_request_method = Some(true);
                }

                if self.error_type.is_none() {
                    self.error_type = Some(true);
                }
                if self.http_response_status_code.is_none() {
                    self.http_response_status_code = Some(true);
                }

                // Recommended
                if self.http_request_body_size.is_none() {
                    self.http_request_body_size = Some(true);
                }

                if self.http_response_body_size.is_none() {
                    self.http_response_body_size = Some(true);
                }
                if self.network_protocol_version.is_none() {
                    self.network_protocol_version = Some(true);
                }
                if self.network_type.is_none() {
                    self.network_type = Some(true);
                }
                if self.user_agent_original.is_none() {
                    self.user_agent_original = Some(true);
                }
            }
            DefaultAttributeRequirementLevel::None => {}
        }
    }
}

#[cfg(test)]
mod test {
    use insta::assert_yaml_snapshot;

    use crate::plugins::telemetry::config_new::attributes::Extendable;
    use crate::plugins::telemetry::config_new::attributes::SupergraphAttributes;
    use crate::plugins::telemetry::config_new::attributes::SupergraphCustomAttribute;

    #[test]
    fn test_extendable_serde() {
        let mut settings = insta::Settings::clone_current();
        settings.set_sort_maps(true);
        settings.bind(|| {
            let o = serde_json::from_value::<
                Extendable<SupergraphAttributes, SupergraphCustomAttribute>,
            >(serde_json::json!({
                    "graphql.operation.name": true,
                    "graphql.operation.type": true,
                    "custom_1": {
                        "operation_name": "string"
                    },
                    "custom_2": {
                        "operation_name": "string"
                    }
            }))
            .unwrap();
            assert_yaml_snapshot!(o);
        });
    }

    #[test]
    fn test_extendable_serde_fail() {
        serde_json::from_value::<Extendable<SupergraphAttributes, SupergraphCustomAttribute>>(
            serde_json::json!({
                    "graphql.operation": true,
                    "graphql.operation.type": true,
                    "custom_1": {
                        "operation_name": "string"
                    },
                    "custom_2": {
                        "operation_name": "string"
                    }
            }),
        )
        .expect_err("Should have errored");
    }
}
