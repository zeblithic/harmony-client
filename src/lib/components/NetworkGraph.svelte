<script lang="ts">
  import { onMount } from 'svelte';
  import {
    forceSimulation,
    forceLink,
    forceManyBody,
    forceCenter,
    forceCollide,
    type Simulation,
    type SimulationNodeDatum,
    type SimulationLinkDatum,
  } from 'd3-force';
  import { zoom, zoomIdentity, type ZoomTransform, type ZoomBehavior } from 'd3-zoom';
  import { select } from 'd3-selection';
  import type { NetworkNode, NetworkLink } from '../network-types';
  import {
    nodeHealthColor,
    linkUtilizationColor,
    linkWidth,
    nodeRadius,
    findNodeAtPoint,
    findLinkAtPoint,
    advanceParticle,
  } from '../graph-utils';

  interface SimNode extends SimulationNodeDatum {
    address: string;
    displayName: string;
    hopDistance: number;
    status: 'online' | 'degraded' | 'offline';
    isLocal: boolean;
  }

  interface SimLink extends SimulationLinkDatum<SimNode> {
    id: string;
    utilizationPercent: number;
  }

  interface Particle {
    linkIndex: number;
    position: number;
    speed: number;
  }

  let {
    nodes,
    links,
    selectedAddress = null,
    onNodeClick,
    onNodeHover,
    onLinkClick,
  }: {
    nodes: NetworkNode[];
    links: NetworkLink[];
    selectedAddress?: string | null;
    onNodeClick?: (address: string) => void;
    onNodeHover?: (address: string | null) => void;
    onLinkClick?: (linkId: string) => void;
  } = $props();

  let canvas: HTMLCanvasElement;
  let simNodes: SimNode[] = [];
  let simLinks: SimLink[] = [];
  let particles: Particle[] = [];
  let hoveredAddress: string | null = null;
  let transform: ZoomTransform = zoomIdentity;
  let simulation: Simulation<SimNode, SimLink> | null = null;
  let zoomBehavior: ZoomBehavior<HTMLCanvasElement, unknown> | null = null;
  let animationFrameId: number = 0;
  let lastFrameTime: number = 0;
  let resizeObserver: ResizeObserver | null = null;

  function createSimNodes(sourceNodes: NetworkNode[]): SimNode[] {
    return sourceNodes.map((n) => ({
      address: n.address,
      displayName: n.displayName,
      hopDistance: n.hopDistance,
      status: n.status,
      isLocal: n.isLocal,
    }));
  }

  function createSimLinks(sourceLinks: NetworkLink[], nodeList: SimNode[]): SimLink[] {
    const nodeMap = new Map(nodeList.map((n) => [n.address, n]));
    return sourceLinks
      .filter((l) => nodeMap.has(l.source) && nodeMap.has(l.target))
      .map((l) => ({
        id: l.id,
        source: nodeMap.get(l.source)!,
        target: nodeMap.get(l.target)!,
        utilizationPercent: l.utilizationPercent,
      }));
  }

  function initParticles(): Particle[] {
    const result: Particle[] = [];
    for (let i = 0; i < simLinks.length; i++) {
      const link = simLinks[i];
      const count = Math.min(8, Math.floor(link.utilizationPercent / 15) + 1);
      for (let j = 0; j < count; j++) {
        if (result.length >= 200) break;
        result.push({
          linkIndex: i,
          position: j / count,
          speed: 0.1 + (link.utilizationPercent / 100) * 0.4,
        });
      }
      if (result.length >= 200) break;
    }
    return result;
  }

  function syncData() {
    // Update existing node statuses (O(n) via Map)
    const nodeSourceMap = new Map(nodes.map((n) => [n.address, n]));
    for (const simNode of simNodes) {
      const source = nodeSourceMap.get(simNode.address);
      if (source) {
        simNode.status = source.status;
      }
    }

    // Update existing link utilization + particle speeds (O(m) via Map)
    const linkSourceMap = new Map(links.map((l) => [l.id, l]));
    for (const simLink of simLinks) {
      const source = linkSourceMap.get(simLink.id);
      if (source) {
        simLink.utilizationPercent = source.utilizationPercent;
      }
    }

    // Update particle speeds to reflect current utilization
    for (const particle of particles) {
      const link = simLinks[particle.linkIndex];
      if (link) {
        particle.speed = 0.1 + (link.utilizationPercent / 100) * 0.4;
      }
    }

    // Detect new nodes
    if (nodes.length > simNodes.length) {
      simNodes = createSimNodes(nodes);
      simLinks = createSimLinks(links, simNodes);
      particles = initParticles();
      if (simulation) {
        simulation.nodes(simNodes);
        const linkForce = simulation.force('link') as ReturnType<typeof forceLink<SimNode, SimLink>> | undefined;
        if (linkForce) {
          linkForce.links(simLinks);
        }
        simulation.alpha(0.3).restart();
      }
    } else if (links.length > simLinks.length) {
      simLinks = createSimLinks(links, simNodes);
      particles = initParticles();
      if (simulation) {
        const linkForce = simulation.force('link') as ReturnType<typeof forceLink<SimNode, SimLink>> | undefined;
        if (linkForce) {
          linkForce.links(simLinks);
        }
        simulation.alpha(0.3).restart();
      }
    }
  }

  function drawBackground(ctx: CanvasRenderingContext2D, w: number, h: number) {
    const gradient = ctx.createRadialGradient(w / 2, h / 2, 0, w / 2, h / 2, Math.max(w, h) / 2);
    gradient.addColorStop(0, '#1e1f22');
    gradient.addColorStop(1, '#1a1b1e');
    ctx.fillStyle = gradient;
    ctx.fillRect(0, 0, w, h);
  }

  function drawLinks(ctx: CanvasRenderingContext2D) {
    for (const link of simLinks) {
      const source = link.source as SimNode;
      const target = link.target as SimNode;
      if (source.x == null || source.y == null || target.x == null || target.y == null) continue;

      ctx.beginPath();
      ctx.moveTo(source.x, source.y);
      ctx.lineTo(target.x, target.y);
      ctx.strokeStyle = linkUtilizationColor(link.utilizationPercent);
      ctx.lineWidth = linkWidth(link.utilizationPercent);
      ctx.stroke();
    }
  }

  function drawParticles(ctx: CanvasRenderingContext2D) {
    for (const particle of particles) {
      const link = simLinks[particle.linkIndex];
      if (!link) continue;
      const source = link.source as SimNode;
      const target = link.target as SimNode;
      if (source.x == null || source.y == null || target.x == null || target.y == null) continue;

      const t = particle.position;
      const x = source.x + (target.x - source.x) * t;
      const y = source.y + (target.y - source.y) * t;

      const edgeFade = Math.min(t, 1 - t) * 4;
      const alpha = Math.min(1, edgeFade) * 0.7;

      ctx.beginPath();
      ctx.arc(x, y, 2.5, 0, Math.PI * 2);
      ctx.fillStyle = `rgba(255, 255, 255, ${alpha})`;
      ctx.fill();
    }
  }

  function drawNodes(ctx: CanvasRenderingContext2D) {
    for (const node of simNodes) {
      if (node.x == null || node.y == null) continue;
      const radius = nodeRadius(node.hopDistance);
      const color = nodeHealthColor(node.status, node.isLocal);

      // Selected glow
      if (selectedAddress && node.address === selectedAddress) {
        ctx.save();
        ctx.shadowColor = color;
        ctx.shadowBlur = 16;
        ctx.beginPath();
        ctx.arc(node.x, node.y, radius, 0, Math.PI * 2);
        ctx.fillStyle = color;
        ctx.fill();
        ctx.restore();
      } else {
        ctx.beginPath();
        ctx.arc(node.x, node.y, radius, 0, Math.PI * 2);
        ctx.fillStyle = color;
        ctx.fill();
      }

      // Hovered ring
      if (hoveredAddress && node.address === hoveredAddress) {
        ctx.beginPath();
        ctx.arc(node.x, node.y, radius + 2, 0, Math.PI * 2);
        ctx.strokeStyle = '#ffffff';
        ctx.lineWidth = 2;
        ctx.stroke();
      }

      // Label
      ctx.fillStyle = '#b5bac1';
      ctx.font = '11px sans-serif';
      ctx.textAlign = 'center';
      const label =
        node.displayName.length > 12 ? node.displayName.slice(0, 12) + '...' : node.displayName;
      ctx.fillText(label, node.x, node.y + radius + 14);
    }
  }

  function render(timestamp: number) {
    if (!canvas) return;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;

    const dt = lastFrameTime > 0 ? (timestamp - lastFrameTime) / 1000 : 0.016;
    lastFrameTime = timestamp;

    // Sync data from props each frame
    syncData();

    // Advance particles
    for (const particle of particles) {
      particle.position = advanceParticle(particle.position, particle.speed, dt);
    }

    const dpr = window.devicePixelRatio || 1;
    const w = canvas.width / dpr;
    const h = canvas.height / dpr;

    // Clear and draw background (not transformed)
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    drawBackground(ctx, w, h);

    // Apply zoom transform
    ctx.save();
    ctx.translate(transform.x, transform.y);
    ctx.scale(transform.k, transform.k);

    drawLinks(ctx);
    drawParticles(ctx);
    drawNodes(ctx);

    ctx.restore();

    animationFrameId = requestAnimationFrame(render);
    if (document.hidden) return;
  }

  function screenToWorld(clientX: number, clientY: number): { x: number; y: number } {
    const rect = canvas.getBoundingClientRect();
    const px = (clientX - rect.left - transform.x) / transform.k;
    const py = (clientY - rect.top - transform.y) / transform.k;
    return { x: px, y: py };
  }

  function handleClick(event: MouseEvent) {
    const { x, y } = screenToWorld(event.clientX, event.clientY);
    const hitNode = findNodeAtPoint(x, y, simNodes);
    if (hitNode) {
      onNodeClick?.(hitNode.address);
      return;
    }
    const hitLink = findLinkAtPoint(x, y, simLinks);
    if (hitLink) {
      onLinkClick?.(hitLink.id);
    }
  }

  function handleMouseMove(event: MouseEvent) {
    const { x, y } = screenToWorld(event.clientX, event.clientY);
    const hit = findNodeAtPoint(x, y, simNodes);
    const hitLink = !hit ? findLinkAtPoint(x, y, simLinks) : null;
    const newHovered = hit ? hit.address : null;
    if (newHovered !== hoveredAddress) {
      hoveredAddress = newHovered;
      onNodeHover?.(newHovered);
    }
    canvas.style.cursor = (hit || hitLink) ? 'pointer' : 'default';
  }

  function resizeCanvas() {
    if (!canvas) return;
    const parent = canvas.parentElement;
    if (!parent) return;
    const dpr = window.devicePixelRatio || 1;
    const rect = parent.getBoundingClientRect();
    canvas.width = rect.width * dpr;
    canvas.height = rect.height * dpr;
    canvas.style.width = `${rect.width}px`;
    canvas.style.height = `${rect.height}px`;
    const ctx = canvas.getContext('2d');
    if (ctx) {
      ctx.scale(dpr, dpr);
    }
  }

  export function recenter() {
    if (!canvas || !zoomBehavior) return;
    select<HTMLCanvasElement, unknown>(canvas).call(zoomBehavior.transform, zoomIdentity);
    transform = zoomIdentity;
    if (simulation) {
      simulation.alpha(0.3).restart();
    }
  }

  export function zoomToFit() {
    if (!canvas || !zoomBehavior || simNodes.length === 0) return;

    let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    for (const node of simNodes) {
      if (node.x == null || node.y == null) continue;
      const r = nodeRadius(node.hopDistance);
      minX = Math.min(minX, node.x - r);
      minY = Math.min(minY, node.y - r);
      maxX = Math.max(maxX, node.x + r);
      maxY = Math.max(maxY, node.y + r);
    }

    if (!isFinite(minX)) return;

    const parent = canvas.parentElement;
    if (!parent) return;
    const rect = parent.getBoundingClientRect();
    const padding = 40;
    const bboxW = maxX - minX;
    const bboxH = maxY - minY;
    const scale = Math.min(
      (rect.width - padding * 2) / (bboxW || 1),
      (rect.height - padding * 2) / (bboxH || 1),
      4,
    );
    const clampedScale = Math.max(0.2, Math.min(4, scale));
    const cx = (minX + maxX) / 2;
    const cy = (minY + maxY) / 2;
    const tx = rect.width / 2 - cx * clampedScale;
    const ty = rect.height / 2 - cy * clampedScale;

    const newTransform = zoomIdentity.translate(tx, ty).scale(clampedScale);
    select<HTMLCanvasElement, unknown>(canvas).call(zoomBehavior.transform, newTransform);
    transform = newTransform;
  }

  onMount(() => {
    resizeCanvas();

    // Initialize simulation data
    simNodes = createSimNodes(nodes);
    simLinks = createSimLinks(links, simNodes);
    particles = initParticles();

    const parent = canvas.parentElement;
    const rect = parent ? parent.getBoundingClientRect() : { width: 800, height: 600 };

    // Create force simulation
    simulation = forceSimulation<SimNode>(simNodes)
      .force(
        'link',
        forceLink<SimNode, SimLink>(simLinks)
          .id((d) => d.address)
          .distance(120),
      )
      .force('charge', forceManyBody().strength(-300))
      .force('center', forceCenter(rect.width / 2, rect.height / 2))
      .force('collide', forceCollide<SimNode>((d) => nodeRadius(d.hopDistance) + 4));

    // Set up zoom behavior
    zoomBehavior = zoom<HTMLCanvasElement, unknown>()
      .scaleExtent([0.2, 4])
      .on('zoom', (event) => {
        transform = event.transform;
      });

    select<HTMLCanvasElement, unknown>(canvas).call(zoomBehavior);

    // ResizeObserver
    resizeObserver = new ResizeObserver(() => {
      resizeCanvas();
      if (simulation) {
        const p = canvas.parentElement;
        if (p) {
          const r = p.getBoundingClientRect();
          simulation.force('center', forceCenter(r.width / 2, r.height / 2));
          simulation.alpha(0.1).restart();
        }
      }
    });
    if (parent) {
      resizeObserver.observe(parent);
    }

    // Start animation loop
    lastFrameTime = 0;
    animationFrameId = requestAnimationFrame(render);

    return () => {
      cancelAnimationFrame(animationFrameId);
      simulation?.stop();
      resizeObserver?.disconnect();
    };
  });
</script>

<div class="graph-container">
  <canvas
    bind:this={canvas}
    onclick={handleClick}
    onmousemove={handleMouseMove}
    role="img"
    aria-label="Network topology graph showing {simNodes.length} nodes and {simLinks.length} links"
  ></canvas>
</div>

<style>
  .graph-container {
    flex: 1;
    position: relative;
    overflow: hidden;
  }

  canvas {
    display: block;
    width: 100%;
    height: 100%;
  }
</style>
