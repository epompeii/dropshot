// Copyright 2023 Oxide Computer Company

//! Body-related extractor(s)

use crate::api_description::ApiEndpointParameter;
use crate::api_description::ApiSchemaGenerator;
use crate::api_description::{ApiEndpointBodyContentType, ExtensionMode};
use crate::error::HttpError;
use crate::http_util::http_read_body;
use crate::http_util::CONTENT_TYPE_JSON;
use crate::schema_util::make_subschema_for;
use crate::server::ServerContext;
use crate::ExclusiveExtractor;
use crate::ExtractorMetadata;
use crate::RequestContext;
use async_trait::async_trait;
use bytes::Bytes;
use schemars::schema::InstanceType;
use schemars::schema::SchemaObject;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use std::fmt::Debug;

// TypedBody: body extractor for formats that can be deserialized to a specific
// type.  Only JSON is currently supported.

/// `TypedBody<BodyType>` is an extractor used to deserialize an instance of
/// `BodyType` from an HTTP request body.  `BodyType` is any structure of yours
/// that implements `serde::Deserialize`.  See this module's documentation for
/// more information.
#[derive(Debug)]
pub struct TypedBody<BodyType: JsonSchema + DeserializeOwned + Send + Sync> {
    inner: BodyType,
}

impl<BodyType: JsonSchema + DeserializeOwned + Send + Sync>
    TypedBody<BodyType>
{
    // TODO drop this in favor of Deref?  + Display and Debug for convenience?
    pub fn into_inner(self) -> BodyType {
        self.inner
    }
}

/// Given an HTTP request, attempt to read the body, parse it according
/// to the content type, and deserialize it to an instance of `BodyType`.
async fn http_request_load_body<Context: ServerContext, BodyType>(
    rqctx: &RequestContext<Context>,
    mut request: hyper::Request<hyper::Body>,
) -> Result<TypedBody<BodyType>, HttpError>
where
    BodyType: JsonSchema + DeserializeOwned + Send + Sync,
{
    let server = &rqctx.server;
    let body = http_read_body(
        request.body_mut(),
        server.config.request_body_max_bytes,
    )
    .await?;

    // RFC 7231 §3.1.1.1: media types are case insensitive and may
    // be followed by whitespace and/or a parameter (e.g., charset),
    // which we currently ignore.
    let content_type = request
        .headers()
        .get(http::header::CONTENT_TYPE)
        .map(|hv| {
            hv.to_str().map_err(|e| {
                HttpError::for_bad_request(
                    None,
                    format!("invalid content type: {}", e),
                )
            })
        })
        .unwrap_or(Ok(CONTENT_TYPE_JSON))?;
    let end = content_type.find(';').unwrap_or_else(|| content_type.len());
    let mime_type = content_type[..end].trim_end().to_lowercase();
    let body_content_type =
        ApiEndpointBodyContentType::from_mime_type(&mime_type)
            .map_err(|e| HttpError::for_bad_request(None, e))?;
    let expected_content_type = rqctx.body_content_type.clone();

    use ApiEndpointBodyContentType::*;
    let content: BodyType = match (expected_content_type, body_content_type) {
        (Json, Json) => serde_json::from_slice(&body).map_err(|e| {
            HttpError::for_bad_request(
                None,
                format!("unable to parse JSON body: {}", e),
            )
        })?,
        (UrlEncoded, UrlEncoded) => serde_urlencoded::from_bytes(&body)
            .map_err(|e| {
                HttpError::for_bad_request(
                    None,
                    format!("unable to parse URL-encoded body: {}", e),
                )
            })?,
        (expected, requested) => {
            return Err(HttpError::for_bad_request(
                None,
                format!(
                    "expected content type \"{}\", got \"{}\"",
                    expected.mime_type(),
                    requested.mime_type()
                ),
            ))
        }
    };
    Ok(TypedBody { inner: content })
}

// The `ExclusiveExtractor` implementation for TypedBody<BodyType> describes how
// to construct an instance of `TypedBody<BodyType>` from an HTTP request:
// namely, by reading the request body and parsing it as JSON into type
// `BodyType`.  TODO-cleanup We shouldn't have to use the "'static" bound on
// `BodyType` here.  It seems like we ought to be able to use 'async_trait, but
// that doesn't seem to be defined.
#[async_trait]
impl<BodyType> ExclusiveExtractor for TypedBody<BodyType>
where
    BodyType: JsonSchema + DeserializeOwned + Send + Sync + 'static,
{
    async fn from_request<Context: ServerContext>(
        rqctx: &RequestContext<Context>,
        request: hyper::Request<hyper::Body>,
    ) -> Result<TypedBody<BodyType>, HttpError> {
        http_request_load_body(rqctx, request).await
    }

    fn metadata(content_type: ApiEndpointBodyContentType) -> ExtractorMetadata {
        let body = ApiEndpointParameter::new_body(
            content_type,
            true,
            ApiSchemaGenerator::Gen {
                name: BodyType::schema_name,
                schema: make_subschema_for::<BodyType>,
            },
            vec![],
        );
        ExtractorMetadata {
            extension_mode: ExtensionMode::None,
            parameters: vec![body],
        }
    }
}

// UntypedBody: body extractor for a plain array of bytes of a body.

/// `UntypedBody` is an extractor for reading in the contents of the HTTP request
/// body and making the raw bytes directly available to the consumer.
#[derive(Debug)]
pub struct UntypedBody {
    content: Bytes,
}

impl UntypedBody {
    /// Returns a byte slice of the underlying body content.
    // TODO drop this in favor of Deref?  + Display and Debug for convenience?
    pub fn as_bytes(&self) -> &[u8] {
        &self.content
    }

    /// Convenience wrapper to convert the body to a UTF-8 string slice,
    /// returning a 400-level error if the body is not valid UTF-8.
    pub fn as_str(&self) -> Result<&str, HttpError> {
        std::str::from_utf8(self.as_bytes()).map_err(|e| {
            HttpError::for_bad_request(
                None,
                format!("failed to parse body as UTF-8 string: {}", e),
            )
        })
    }
}

#[async_trait]
impl ExclusiveExtractor for UntypedBody {
    async fn from_request<Context: ServerContext>(
        rqctx: &RequestContext<Context>,
        mut request: hyper::Request<hyper::Body>,
    ) -> Result<UntypedBody, HttpError> {
        let server = &rqctx.server;
        let body_bytes = http_read_body(
            request.body_mut(),
            server.config.request_body_max_bytes,
        )
        .await?;
        Ok(UntypedBody { content: body_bytes })
    }

    fn metadata(
        _content_type: ApiEndpointBodyContentType,
    ) -> ExtractorMetadata {
        ExtractorMetadata {
            parameters: vec![ApiEndpointParameter::new_body(
                ApiEndpointBodyContentType::Bytes,
                true,
                ApiSchemaGenerator::Static {
                    schema: Box::new(
                        SchemaObject {
                            instance_type: Some(InstanceType::String.into()),
                            format: Some(String::from("binary")),
                            ..Default::default()
                        }
                        .into(),
                    ),
                    dependencies: indexmap::IndexMap::default(),
                },
                vec![],
            )],
            extension_mode: ExtensionMode::None,
        }
    }
}
