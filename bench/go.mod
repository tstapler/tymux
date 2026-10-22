module github.com/tstapler/tymux/bench

go 1.26.4

require (
	connectrpc.com/connect v1.20.0
	github.com/tstapler/tymux/clients/go v0.0.0
)

require (
	golang.org/x/net v0.58.0 // indirect
	golang.org/x/text v0.41.0 // indirect
	google.golang.org/protobuf v1.36.12 // indirect
)

replace github.com/tstapler/tymux/clients/go => ../clients/go
