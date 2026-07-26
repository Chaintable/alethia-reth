//! Transport-specific registration for custom Taiko RPC methods.

use jsonrpsee::{Methods, core::RegisterMethodError};
use reth_rpc_builder::TransportRpcModules;

/// Registers the heavy DeBank trace endpoint on HTTP without exposing it on WS or IPC.
pub fn merge_debank_trace_http_only(
    modules: &mut TransportRpcModules,
    methods: impl Into<Methods>,
) -> Result<bool, RegisterMethodError> {
    modules.merge_http(methods)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonrpsee::RpcModule;

    const DEBANK_TRACE_METHOD: &str = "trace_debankBlock";

    #[tokio::test]
    async fn debank_trace_is_registered_only_on_http() {
        let mut modules = TransportRpcModules::default()
            .with_http(RpcModule::new(()))
            .with_ws(RpcModule::new(()))
            .with_ipc(RpcModule::new(()));
        let mut debank_trace = RpcModule::new(());
        debank_trace
            .register_method(DEBANK_TRACE_METHOD, |_, _, _| "unused")
            .expect("test method should register");

        assert!(
            merge_debank_trace_http_only(&mut modules, debank_trace)
                .expect("HTTP-only merge should succeed")
        );

        let http = modules.http_methods(|_| true).expect("HTTP module should exist");
        assert!(http.method_names().any(|name| name == DEBANK_TRACE_METHOD));

        let request = r#"{"jsonrpc":"2.0","id":1,"method":"trace_debankBlock","params":["0x1"]}"#;
        for methods in [
            modules.ws_methods(|_| true).expect("WS module should exist"),
            modules.ipc_methods(|_| true).expect("IPC module should exist"),
        ] {
            assert!(!methods.method_names().any(|name| name == DEBANK_TRACE_METHOD));
            let (response, _) =
                methods.raw_json_request(request, 1).await.expect("request should parse");
            assert!(
                response.get().contains(r#""code":-32601"#) &&
                    response.get().contains(r#""message":"Method not found""#),
                "unregistered transport must return method not found: {}",
                response.get()
            );
        }
    }
}
