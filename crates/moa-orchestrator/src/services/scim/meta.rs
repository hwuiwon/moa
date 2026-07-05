//! Static SCIM metadata endpoints.

use axum::Json;
use serde_json::{Value, json};

/// Return SCIM service-provider capabilities.
pub async fn service_provider_config() -> Json<Value> {
    Json(json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"],
        "patch": { "supported": true },
        "bulk": { "supported": false, "maxOperations": 0, "maxPayloadSize": 0 },
        "filter": { "supported": true, "maxResults": 200 },
        "changePassword": { "supported": false },
        "sort": { "supported": false },
        "etag": { "supported": true },
        "authenticationSchemes": [{
            "type": "oauthbearertoken",
            "name": "OAuth Bearer Token",
            "description": "MOA API key with tenant admin access",
            "primary": true
        }],
        "meta": {
            "resourceType": "ServiceProviderConfig",
            "location": "/scim/v2/ServiceProviderConfig"
        }
    }))
}

/// Return supported SCIM resource types.
pub async fn resource_types() -> Json<Value> {
    Json(json!({
        "schemas": ["urn:ietf:params:scim:api:messages:2.0:ListResponse"],
        "totalResults": 2,
        "itemsPerPage": 2,
        "startIndex": 1,
        "Resources": [
            {
                "id": "User",
                "name": "User",
                "endpoint": "/Users",
                "schema": "urn:ietf:params:scim:schemas:core:2.0:User",
                "meta": { "resourceType": "ResourceType", "location": "/scim/v2/ResourceTypes/User" }
            },
            {
                "id": "Group",
                "name": "Group",
                "endpoint": "/Groups",
                "schema": "urn:ietf:params:scim:schemas:core:2.0:Group",
                "meta": { "resourceType": "ResourceType", "location": "/scim/v2/ResourceTypes/Group" }
            }
        ]
    }))
}

/// Return the SCIM core schemas implemented by MOA.
pub async fn schemas() -> Json<Value> {
    Json(json!({
        "schemas": ["urn:ietf:params:scim:api:messages:2.0:ListResponse"],
        "totalResults": 2,
        "itemsPerPage": 2,
        "startIndex": 1,
        "Resources": [
            {
                "id": "urn:ietf:params:scim:schemas:core:2.0:User",
                "name": "User",
                "description": "User Account",
                "attributes": [
                    { "name": "userName", "type": "string", "multiValued": false, "required": true },
                    { "name": "active", "type": "boolean", "multiValued": false },
                    { "name": "emails", "type": "complex", "multiValued": true },
                    { "name": "name", "type": "complex", "multiValued": false },
                    { "name": "displayName", "type": "string", "multiValued": false }
                ],
                "meta": { "resourceType": "Schema", "location": "/scim/v2/Schemas/urn:ietf:params:scim:schemas:core:2.0:User" }
            },
            {
                "id": "urn:ietf:params:scim:schemas:core:2.0:Group",
                "name": "Group",
                "description": "Group",
                "attributes": [
                    { "name": "displayName", "type": "string", "multiValued": false, "required": true },
                    { "name": "members", "type": "complex", "multiValued": true }
                ],
                "meta": { "resourceType": "Schema", "location": "/scim/v2/Schemas/urn:ietf:params:scim:schemas:core:2.0:Group" }
            }
        ]
    }))
}
