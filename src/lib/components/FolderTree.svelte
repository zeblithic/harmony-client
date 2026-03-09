<script lang="ts">
  import type { ContentItem } from '../types';

  let {
    items = [],
    onFolderSelect,
    selectedCid,
  }: {
    items?: ContentItem[];
    onFolderSelect?: (cid: string | null) => void;
    selectedCid?: string | null;
  } = $props();

  let expandedCids = $state<Set<string>>(new Set());

  /** Root-level folders (parentCid === null and isFolder === true). */
  let rootFolders = $derived(items.filter((i) => i.parentCid === null && i.isFolder));

  /** Get children of a given parent CID. */
  function getChildren(parentCid: string): ContentItem[] {
    return items.filter((i) => i.parentCid === parentCid);
  }

  function toggleFolder(cid: string) {
    const next = new Set(expandedCids);
    if (next.has(cid)) {
      next.delete(cid);
    } else {
      next.add(cid);
    }
    expandedCids = next;
    onFolderSelect?.(cid);
  }

  function selectRoot() {
    onFolderSelect?.(null);
  }

  function handleFolderKeydown(event: KeyboardEvent, cid: string) {
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      toggleFolder(cid);
    }
  }

  function handleRootKeydown(event: KeyboardEvent) {
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      selectRoot();
    }
  }
</script>

<nav class="folder-tree" aria-label="Folder tree">
  <ul role="tree">
    <li role="treeitem" aria-selected={selectedCid === null}>
      <button
        type="button"
        class="folder-btn root-btn"
        aria-current={selectedCid === null ? 'true' : undefined}
        onclick={selectRoot}
        onkeydown={handleRootKeydown}
      >
        All Files
      </button>
    </li>
    {#each rootFolders as folder (folder.cid)}
      {@const isExpanded = expandedCids.has(folder.cid)}
      <li role="treeitem" aria-selected={selectedCid === folder.cid}>
        <button
          type="button"
          class="folder-btn"
          class:selected={selectedCid === folder.cid}
          aria-expanded={isExpanded ? 'true' : 'false'}
          aria-current={selectedCid === folder.cid ? 'true' : undefined}
          onclick={() => toggleFolder(folder.cid)}
          onkeydown={(e) => handleFolderKeydown(e, folder.cid)}
        >
          <span class="folder-icon">{isExpanded ? '▼' : '▶'}</span>
          {folder.name}
        </button>
        {#if isExpanded}
          {@const children = getChildren(folder.cid)}
          <ul role="group" class="folder-children">
            {#each children as child (child.cid)}
              <li role="treeitem" aria-selected={selectedCid === child.cid}>
                {#if child.isFolder}
                  {@const childExpanded = expandedCids.has(child.cid)}
                  <button
                    type="button"
                    class="folder-btn child-btn"
                    class:selected={selectedCid === child.cid}
                    aria-expanded={childExpanded ? 'true' : 'false'}
                    aria-current={selectedCid === child.cid ? 'true' : undefined}
                    onclick={() => toggleFolder(child.cid)}
                    onkeydown={(e) => handleFolderKeydown(e, child.cid)}
                  >
                    <span class="folder-icon">{childExpanded ? '▼' : '▶'}</span>
                    {child.name}
                  </button>
                  {#if childExpanded}
                    {@const grandchildren = getChildren(child.cid)}
                    <ul role="group" class="folder-children">
                      {#each grandchildren as grandchild (grandchild.cid)}
                        <li role="treeitem" aria-selected={selectedCid === grandchild.cid}>
                          {#if grandchild.isFolder}
                            <button
                              type="button"
                              class="folder-btn child-btn"
                              class:selected={selectedCid === grandchild.cid}
                              aria-current={selectedCid === grandchild.cid ? 'true' : undefined}
                              onclick={() => onFolderSelect?.(grandchild.cid)}
                              onkeydown={(e) => handleFolderKeydown(e, grandchild.cid)}
                            >
                              <span class="folder-icon">{'\u{1F4C1}'}</span>
                              {grandchild.name}
                            </button>
                          {:else}
                            <span class="file-entry">{grandchild.name}</span>
                          {/if}
                        </li>
                      {/each}
                    </ul>
                  {/if}
                {:else}
                  <span class="file-entry">{child.name}</span>
                {/if}
              </li>
            {/each}
          </ul>
        {/if}
      </li>
    {/each}
  </ul>
</nav>

<style>
  .folder-tree {
    padding: 4px 0;
  }

  [role="tree"] {
    list-style: none;
    margin: 0;
    padding: 0;
  }

  [role="group"] {
    list-style: none;
    margin: 0;
    padding: 0 0 0 16px;
  }

  .folder-btn {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 4px 8px;
    border: none;
    border-radius: 4px;
    background: none;
    color: var(--text-primary, #f2f3f5);
    font-size: 0.85rem;
    cursor: pointer;
    text-align: left;
  }

  .folder-btn:hover {
    background: var(--bg-tertiary, #313338);
  }

  .folder-btn.selected,
  .folder-btn[aria-current="true"] {
    background: var(--accent, #5865f2);
    color: #fff;
  }

  .folder-icon {
    font-size: 0.65rem;
    width: 12px;
    flex-shrink: 0;
  }

  .root-btn {
    font-weight: 600;
  }

  .file-entry {
    display: block;
    padding: 3px 8px 3px 24px;
    font-size: 0.8rem;
    color: var(--text-secondary, #b5bac1);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
</style>
