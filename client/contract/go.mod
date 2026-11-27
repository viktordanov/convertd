// Contract tests and spec tools for the Convert API. This module is never
// published or tagged; it exists so that kin-openapi stays out of the client
// module's go.mod.
module github.com/viktordanov/rs-convert/client/contract

go 1.26.0

require (
	github.com/getkin/kin-openapi v0.153.0
	github.com/klauspost/compress v1.20.5
	github.com/viktordanov/rs-convert/client v0.0.0
	go.yaml.in/yaml/v4 v4.0.0
)

require (
	github.com/apapsch/go-jsonmerge/v2 v2.0.0 // indirect
	github.com/go-openapi/jsonpointer v0.22.5 // indirect
	github.com/go-openapi/swag/jsonname v0.25.5 // indirect
	github.com/google/uuid v1.6.0 // indirect
	github.com/oapi-codegen/runtime v1.9.1 // indirect
	github.com/santhosh-tekuri/jsonschema/v6 v6.0.3 // indirect
	golang.org/x/text v0.41.0 // indirect
)

replace github.com/viktordanov/rs-convert/client => ../
