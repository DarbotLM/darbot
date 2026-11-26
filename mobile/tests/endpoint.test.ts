import assert from "node:assert/strict";
import test from "node:test";
import { normalizeEndpoint, isSameOrigin, isExternalHttps } from "../src/endpoint.ts";
test("normalizes a trusted HTTPS origin", () => {
  assert.equal(normalizeEndpoint(" Example.org:8443 "), "https://example.org:8443");
  assert.equal(normalizeEndpoint("https://example.org/"), "https://example.org");
});
test("rejects cleartext, credentials and non-origin URLs", () => {
  for (const url of ["http://example.org", "https://a:b@example.org", "https://example.org/x", "https://example.org/?token=x", "https://example.org/#x", "file:///tmp/x"])
    assert.throws(() => normalizeEndpoint(url));
});
test("only the exact configured HTTPS origin is trusted", () => {
  const origin = "https://example.org";
  assert.equal(isSameOrigin(origin + "/channel/123", origin), true);
  for (const url of ["https://example.org.evil.net", "https://example.org:8443", "http://example.org", "javascript:alert(1)", "https://user@example.org"])
    assert.equal(isSameOrigin(url, origin), false);
  assert.equal(isExternalHttps("https://docs.example.org"), true);
  assert.equal(isExternalHttps("intent://example.org"), false);
});
