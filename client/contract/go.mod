// Contract tests and spec tools for the Convert API. This module is never
// published or tagged; it exists so that kin-openapi stays out of the client
// module's go.mod.
module github.com/viktordanov/rs-convert/client/contract

go 1.26.0

require github.com/viktordanov/rs-convert/client v0.0.0

require (
	github.com/apapsch/go-jsonmerge/v2 v2.0.0 // indirect
	github.com/google/uuid v1.6.0 // indirect
	github.com/oapi-codegen/runtime v1.9.1 // indirect
)

replace github.com/viktordanov/rs-convert/client => ../
