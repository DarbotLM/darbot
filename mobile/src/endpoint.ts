// Keep the WebView on a user-selected HTTPS origin; never put credentials in a URL.
export function normalizeEndpoint(value: string): string {
  const input = value.trim();
  const url = new URL(input.includes("://") ? input : "https://" + input);
  if (url.protocol !== "https:" || !url.hostname || url.username || url.password ||
      url.pathname !== "/" || url.search || url.hash) {
    throw new Error("Enter an HTTPS server origin, without a path, credentials, or query.");
  }
  return url.origin;
}
export function isSameOrigin(url: string, origin: string): boolean {
  try {
    const target = new URL(url);
    return target.protocol === "https:" && !target.username && !target.password &&
      target.origin === origin;
  } catch { return false; }
}
export function isExternalHttps(url: string): boolean {
  try {
    const target = new URL(url);
    return target.protocol === "https:" && !target.username && !target.password;
  } catch { return false; }
}
