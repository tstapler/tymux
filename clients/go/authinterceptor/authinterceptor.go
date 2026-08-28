// Package authinterceptor provides a connect-go interceptor that
// attaches a bearer token to every outgoing call, unary and streaming
// alike. Shared by the integration tests and both example binaries so
// the auth-header logic exists in exactly one place.
package authinterceptor

import (
	"context"

	"connectrpc.com/connect"
)

// Interceptor sets Authorization: Bearer <token> on both unary and
// streaming client calls. connect-go's convenience UnaryInterceptorFunc
// only implements WrapUnary — its WrapStreamingClient is a documented
// no-op (verified against connectrpc.com/connect@v1.20.0), which would
// silently exempt Attach from auth. Implementing the full
// connect.Interceptor interface here avoids that gap. An empty Token
// is a no-op, matching every other client stack's "empty is absent"
// treatment.
type Interceptor struct {
	Token string
}

func (a Interceptor) WrapUnary(next connect.UnaryFunc) connect.UnaryFunc {
	return func(ctx context.Context, req connect.AnyRequest) (connect.AnyResponse, error) {
		if a.Token != "" {
			req.Header().Set("Authorization", "Bearer "+a.Token)
		}
		return next(ctx, req)
	}
}

func (a Interceptor) WrapStreamingClient(next connect.StreamingClientFunc) connect.StreamingClientFunc {
	return func(ctx context.Context, spec connect.Spec) connect.StreamingClientConn {
		conn := next(ctx, spec)
		if a.Token != "" {
			conn.RequestHeader().Set("Authorization", "Bearer "+a.Token)
		}
		return conn
	}
}

func (a Interceptor) WrapStreamingHandler(next connect.StreamingHandlerFunc) connect.StreamingHandlerFunc {
	return next // client-only; no server-side handler wrapping needed here
}
