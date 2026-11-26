# Darbot Mobile (iOS and Android)

A native Expo/React Native host for the existing authenticated Darbot web application. This is a shared-client architecture, not a rewrite of the agent runtime. The application loads a user-selected HTTPS Darbot deployment in WKWebView / Android WebView. No native bridge or injected JavaScript is provided to the remote page. Same-origin API calls, cookies, streaming, channel history and agent routing remain owned by Darbot. The web UI in app/ contains the phone layout.

## Development

Use Node 22.18+ and npm, with the Android SDK or macOS/Xcode for native builds. The mobile package intentionally lives outside the Bun workspace.

```sh
cd mobile
npm install
npm run typecheck
npm test
npx expo start
```

Open in a compatible Expo Go client or generate native projects using `npx expo prebuild` then `npm run android` or (macOS) `npm run ios`. For a standalone internal build, configure your EAS account and run `npx eas-cli build --platform android --profile preview` or `npx eas-cli build --platform ios --profile preview`. iOS signing and registered devices are required. Production uses your own Apple and Google developer accounts. Generate and commit a lockfile after resolving the pinned SDK dependencies; do not interpret the manifest as a verified lockfile.

Deploy the updated Darbot web app first, then enter the HTTPS origin in the native app. The device must be able to reach the host, including over VPN when needed.

## Validation gates

- Verify local sign-in and sign-out, conversation creation/history, streaming and cancellation, agent mentions, attachment selection, A2UI cards, skills/routines, agent detail, and computer-view panels with a real deployment.
- Exercise small and large phones, tablets, rotation, keyboard opening, large font, screen reader, back navigation and interrupted Wi-Fi on both OSes.
- Verify certificate errors fail closed, top-level origin restrictions, popup links, session-cookie isolation, download/file picker behavior, and untrusted embedded content.
- External top-level HTTPS links require confirmation and open in the system browser. External OAuth/SSO callbacks are not implemented in this host; embedded user agents may be rejected by identity providers. Deployments requiring SSO need a system-browser auth session and secure callback/session exchange before production.
- Native voice, push notifications, offline sync, platform share-sheet ingestion, branded launcher assets, native deep links and store review are not implemented. The web interface does not acquire native capabilities merely by being hosted in a WebView.
- Release only after signed physical-device builds and real deployment integration pass. No live server, build or store publication is implied by this source.

The black mobile surfaces use the existing desktop brand palette; desktop layouts and saved explicit theme preferences remain intact.
