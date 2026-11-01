# The map of lokode

One engine runs the turns.
The terminal screen, an editor and `lokode serve` are 3 views of it.
A browser page and an attached terminal follow a session of `lokode serve`.  
Commands run inside the sandbox on Linux and macOS, and an agent in the background that writes works in a copy of the project.

<svg class="map" viewBox="0 0 880 520" role="img" aria-label="The interfaces around the engine, an attached terminal through lokode serve, the sandbox around the commands and the copy a background agent works in">
  <g class="edge">
    <path d="M150 90 L330 190"/><path d="M150 190 L330 215"/><path d="M150 290 L330 240"/><path d="M90 360 L90 320"/>
    <path d="M550 215 L640 130"/><path d="M550 240 L640 240"/><path d="M550 265 L640 350"/>
  </g>
  <rect class="box" x="30" y="60" width="120" height="60" rx="10"/><text x="90" y="86" text-anchor="middle">Screen</text><text class="dim" x="90" y="104" text-anchor="middle">lokode</text>
  <rect class="box" x="30" y="160" width="120" height="60" rx="10"/><text x="90" y="186" text-anchor="middle">Editor</text><text class="dim" x="90" y="204" text-anchor="middle">lokode acp</text>
  <rect class="box" x="30" y="260" width="120" height="60" rx="10"/><text x="90" y="286" text-anchor="middle">Browser</text><text class="dim" x="90" y="304" text-anchor="middle">lokode serve</text>
  <rect class="box" x="30" y="360" width="120" height="60" rx="10"/><text x="90" y="386" text-anchor="middle">Attached</text><text class="dim" x="90" y="404" text-anchor="middle">lokode attach</text>
  <rect class="core" x="330" y="170" width="220" height="120" rx="14"/>
  <text x="440" y="208" text-anchor="middle" style="font-weight:600;font-size:16px">The engine</text>
  <text class="dim" x="440" y="232" text-anchor="middle">turns, tools, approvals</text>
  <text class="dim" x="440" y="250" text-anchor="middle">sessions, undo, roles</text>
  <text class="dim" x="440" y="268" text-anchor="middle">hooks, plugins, MCP</text>
  <rect class="fence" x="630" y="60" width="220" height="100" rx="14"/>
  <text x="740" y="40" text-anchor="middle" class="dim">sandbox, on Linux and macOS</text>
  <rect class="box" x="650" y="80" width="180" height="60" rx="10"/><text x="740" y="106" text-anchor="middle">Commands</text><text class="dim" x="740" y="124" text-anchor="middle">writes project, temp, caches</text>
  <rect class="box" x="650" y="210" width="180" height="60" rx="10"/><text x="740" y="236" text-anchor="middle">The model</text><text class="dim" x="740" y="254" text-anchor="middle">loken, ollama, a provider</text>
  <rect class="fence" x="630" y="320" width="220" height="150" rx="14"/>
  <text x="740" y="312" text-anchor="middle" class="dim">a copy of the project</text>
  <rect class="box" x="650" y="340" width="180" height="44" rx="10"/><text x="740" y="367" text-anchor="middle">A background agent</text>
  <rect class="box" x="650" y="400" width="180" height="50" rx="10"/><text x="740" y="422" text-anchor="middle">Review and proof</text><text class="dim" x="740" y="438" text-anchor="middle">the caller merges, then lands it</text>
</svg>
<p class="cap">Dashed outlines confine work: the sandbox around commands, the copy around a background agent that writes.</p>

## Where to read more

- The 4 ways in: [Editors, browser and terminal](guides/interfaces.md).
- The sandbox and the approvals: [Approvals and the sandbox](guides/safety.md).
- The copy an agent works in: [Plans and agents](guides/agents.md).
- The models behind the roles: [Models and roles](guides/models.md).
