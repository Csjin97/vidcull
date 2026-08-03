<script lang="ts">
  import { onMount } from "svelte";

  onMount(async () => {
    await import("../../playground");
  });
</script>

<svelte:head>
  <title>vidcull — Ferrari Design MD tokens</title>
</svelte:head>

<header class="pg-head">
  <h1>Ferrari Design MD</h1>
  <p>
    Token playground for vidcull (Session 10.2). Every value is read live from
    <code>tokens.css</code>; the token-consistency test keeps it in sync with
    docs/design-system.md.
  </p>
</header>
<div id="playground"></div>
