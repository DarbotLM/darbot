# Sidepane layers

The desktop sidepane is described as a stack of named layers rather than one
hand-written column, so a surface can compose the layers it needs without
duplicating the agent navigator, its disclosure behaviour, or the tool rail.

The spec lives in [`desktop/src/sidepane-layers.ts`](../desktop/src/sidepane-layers.ts)
and is covered by `desktop/src/sidepane-layers.test.ts`.

## The contract

A sidepane is an `id`, a `label` used as the column's accessible name, and one
or more layers. Each layer declares:

| Field | Meaning |
| --- | --- |
| `id` | Stable identity, unique within the sidepane. Also namespaces saved disclosure state. |
| `title` | Accessible label for the layer. |
| `region` | `header`, `body`, or `footer`. |
| `order` | Ascending placement within the region. Defaults to declaration order. |
| `collapsible` | Whether the layer renders groups that expand and collapse. |
| `expandedByDefault` | Whether a collapsible group starts open when the person has made no choice. |

`defineSidepane` validates the description and returns frozen layers sorted by
region and then by `order`. It rejects unsupported fields, duplicate or unsafe
IDs (lowercase letters, numbers, hyphens and underscores only), unknown
regions, non-negative non-integer orders, non-boolean flags, and layers that
default to expanded without being collapsible. `sidepaneRegion` and
`sidepaneLayer` read the result back.

## Composing a sidepane

```ts
export const COPILOT_WORKSPACE_SIDEPANE = defineSidepane({
  id: "copilot-workspace",
  label: "Agents and chats",
  layers: [
    { id: "brand", title: "Darbot", region: "header", order: 0 },
    { id: "compose", title: "Start work", region: "header", order: 1 },
    {
      id: "agents",
      title: "Agents",
      region: "body",
      order: 0,
      collapsible: true,
      expandedByDefault: false,
    },
    { id: "tools", title: "Workspace tools", region: "footer", order: 0 },
  ],
});
```

The workspace now renders each region from this description and requires a
renderer for every declared layer. Changing a layer's position changes its
placement in the column, rather than only updating metadata. This release has
one fixed, validated composition; user-selectable layouts are not available
yet. A future canvas inspector or fleet column can declare its own layers
against the contract while supplying its own renderers. Layers are data, not
components. The workspace sidepane is also part of the
[desktop UI semantic object model](copilot-cli.md#desktop-ui-semantic-object-model).

## Disclosure

Most agents run a single conversation, so the `agents` layer collapses its
conversations by default. Click the agent icon to open its dialog, then choose
**Show chats** or **Hide chats**. Resolution order in
`isSidepaneGroupExpanded` is:

1. A non-collapsible layer is always open.
2. An explicit saved choice for that group wins, in either direction.
3. Otherwise the group follows `expandedByDefault`, or opens because it is
   active, unless the person explicitly collapses it.

`setSidepaneGroupExpanded` records only explicit choices, keyed by
`layerId:groupId` so two layers never collide, and keeps at most 512 entries by
dropping the oldest. Non-collapsible layers record nothing.

Widget rows use smaller icons and lightly overlapping background layers.
Each row has a separate visual stacking order; hover or keyboard focus raises
it, with overlap confined to row padding so neighboring controls remain
reachable. These visual layers do not create new agent runtimes, change
conversation ownership, or alter the named sidepane-layer/disclosure contract.
The wide widget retains the agents column; portrait widgets hide the column
on narrow viewports and keep agent selection on the live-tile face.

## Storage

Disclosure state is saved under `darbot:copilot:sidepane` as
`{ schemaVersion, groups }`. `validateSidepaneDisclosure` rejects unsupported
fields, an unknown schema version, non-boolean values, malformed namespaced
keys, and more than 512 groups. Existing `agents:<agentId>` choices retain
their keys across this hardening.
An agent with an out-of-contract ID can still expand its group in the current
window; that particular choice is not persisted, and the omission is logged
without discarding valid saved choices.

This is presentation state only. `readSidepaneDisclosure` logs invalid or
unavailable storage and falls back to the defaults rather than failing the
surface. A failed write is logged and leaves the column working, but the choice
will not survive a restart. Conversations and drafts are unaffected; those live
in the separate workspace store described in
[GitHub Copilot CLI runtime](copilot-cli.md).
