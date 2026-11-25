---
type: feature
---

- `loreserver`: `[server.client_policy] min_build` makes the public gRPC endpoint refuse a client whose `User-Agent` build name is not `<build_prefix><N>` with `N >= min_build` (default prefix `ooto-main-`). A refused client gets `FAILED_PRECONDITION` carrying `update_message`, which is where the install link goes. Unset, nothing changes; `[server.grpc_internal]` is never gated. The fork's client release workflow now stamps the tag into the binary, so a release reports `<version>+ooto-main-N`.
