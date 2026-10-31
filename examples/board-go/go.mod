module github.com/jrobhoward/bulkhead/examples/board-go

go 1.27.3

require github.com/jrobhoward/bulkhead/sdks/go v0.0.0

require (
	github.com/fxamacker/cbor/v2 v2.10.0 // indirect
	github.com/x448/float16 v0.8.4 // indirect
	go.bytecodealliance.org/cm v0.3.0 // indirect
)

replace github.com/jrobhoward/bulkhead/sdks/go => ../../sdks/go
