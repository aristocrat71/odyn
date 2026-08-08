import type { GraphNode } from "./api";
import { el } from "./dom";
import { state } from "./state";

const SVG = "http://www.w3.org/2000/svg";
const MIN_ZOOM = 0.15;
const MAX_ZOOM = 6;
// Labels appear from this zoom on.
const LABEL_ZOOM = 0.8;
// The layout fits [-450, 450]; the viewBox leaves a margin around it.
const VIEW = 470;

// The camera survives redraws and mode flips.
let scale = 1;
let tx = 0;
let ty = 0;
let pinned: number | null = null;

export function renderGraph(): HTMLElement {
  const wrap = el("div", "graph");
  const graph = state.brain.graph;
  // DESIGN.md §9: the layout runs in Rust, and this line is its only spinner.
  if (graph === null) {
    wrap.append(el("div", "brain-empty", laying()));
    return wrap;
  }
  if (graph.nodes.length === 0) {
    wrap.append(
      el(
        "div",
        "brain-empty",
        "the ravens haven't returned yet — drop an .md note into the brain folder, or add one in list mode",
      ),
    );
    return wrap;
  }

  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("viewBox", `${-VIEW} ${-VIEW} ${VIEW * 2} ${VIEW * 2}`);
  svg.setAttribute("preserveAspectRatio", "xMidYMid meet");
  svg.classList.add("graph-svg");
  const world = document.createElementNS(SVG, "g");
  svg.append(world);

  const tip = el("div", "graph-tip");
  tip.hidden = true;

  const at = new Map(graph.nodes.map((node) => [node.id, node]));
  for (const edge of graph.edges) {
    const a = at.get(edge.a);
    const b = at.get(edge.b);
    if (a === undefined || b === undefined) continue;
    const line = document.createElementNS(SVG, "line");
    line.setAttribute("x1", String(a.x));
    line.setAttribute("y1", String(a.y));
    line.setAttribute("x2", String(b.x));
    line.setAttribute("y2", String(b.y));
    const kinds = {
      link: "gedge-link",
      similarity: "gedge-sim",
      coinjection: "gedge-co",
    } as const;
    line.classList.add(kinds[edge.kind]);
    world.append(line);
  }
  for (const node of graph.nodes) {
    world.append(dot(node, tip, svg, world));
  }

  const apply = (): void => {
    world.setAttribute("transform", `translate(${tx} ${ty}) scale(${scale})`);
    world.classList.toggle("labeled", scale >= LABEL_ZOOM);
  };
  apply();

  // A screen point in viewBox coordinates, for anchoring the zoom.
  const point = (event: { clientX: number; clientY: number }): DOMPoint => {
    const matrix = svg.getScreenCTM();
    const raw = new DOMPoint(event.clientX, event.clientY);
    return matrix === null ? raw : raw.matrixTransform(matrix.inverse());
  };
  const zoom = (factor: number, anchor: DOMPoint): void => {
    const next = Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, scale * factor));
    tx = anchor.x - ((anchor.x - tx) / scale) * next;
    ty = anchor.y - ((anchor.y - ty) / scale) * next;
    scale = next;
    apply();
  };

  svg.addEventListener("wheel", (event) => {
    event.preventDefault();
    zoom(Math.exp(-event.deltaY * 0.002), point(event));
  });

  let dragging: { x: number; y: number } | null = null;
  svg.addEventListener("pointerdown", (event) => {
    dragging = point(event);
    svg.setPointerCapture(event.pointerId);
  });
  svg.addEventListener("pointermove", (event) => {
    if (dragging === null) return;
    const now = point(event);
    tx += now.x - dragging.x;
    ty += now.y - dragging.y;
    dragging = now;
    apply();
  });
  svg.addEventListener("pointerup", (event) => {
    dragging = null;
    svg.releasePointerCapture(event.pointerId);
  });

  const controls = el("div", "graph-controls");
  const button = (label: string, act: () => void): HTMLButtonElement => {
    const control = el("button", "graph-control", label);
    control.addEventListener("click", act);
    return control;
  };
  const center = new DOMPoint(0, 0);
  controls.append(
    button("+", () => zoom(1.4, center)),
    button("−", () => zoom(1 / 1.4, center)),
    button("fit", () => {
      scale = 1;
      tx = 0;
      ty = 0;
      apply();
    }),
  );

  const hud = el(
    "div",
    "graph-hud",
    "◈ memory   ━ linked   — similar   ┄ co-injected   ·   scroll to zoom · drag to pan",
  );

  wrap.append(svg, hud, controls, tip);
  return wrap;
}

// The count is unknown until the overview lands, so it is left out.
function laying(): string {
  const overview = state.brain.overview;
  if (overview === null) return "laying out memories…";
  return `laying out ${overview.count} memories…`;
}

function dot(
  node: GraphNode,
  tip: HTMLElement,
  svg: SVGSVGElement,
  world: SVGGElement,
): SVGGElement {
  const group = document.createElementNS(SVG, "g");
  group.classList.add("gnode", "gnode-epi");
  const radius = 5.5 + Math.min(node.hits, 14) * 0.45;
  const circle = document.createElementNS(SVG, "circle");
  circle.setAttribute("cx", String(node.x));
  circle.setAttribute("cy", String(node.y));
  circle.setAttribute("r", String(radius));
  const label = document.createElementNS(SVG, "text");
  label.setAttribute("x", String(node.x));
  label.setAttribute("y", String(node.y - radius - 5));
  label.textContent = node.display_id;
  group.append(circle, label);

  const show = (): void => {
    tip.replaceChildren(
      el("div", "graph-tip-id epi", node.display_id),
      el("div", "graph-tip-text", node.content),
      el(
        "div",
        "graph-tip-meta",
        `${node.hits} ${node.hits === 1 ? "hit" : "hits"}`,
      ),
    );
    const box = svg.getBoundingClientRect();
    const matrix = svg.getScreenCTM();
    if (matrix !== null) {
      const spot = new DOMPoint(node.x, node.y).matrixTransform(
        world.getScreenCTM() ?? matrix,
      );
      tip.style.left = `${spot.x - box.x + 14}px`;
      tip.style.top = `${spot.y - box.y + 14}px`;
    }
    tip.hidden = false;
  };
  group.addEventListener("pointerenter", show);
  group.addEventListener("pointerleave", () => {
    if (pinned !== node.id) tip.hidden = true;
  });
  group.addEventListener("click", (event) => {
    event.stopPropagation();
    pinned = pinned === node.id ? null : node.id;
    if (pinned === node.id) show();
    else tip.hidden = true;
  });
  group.addEventListener("dblclick", (event) => {
    event.stopPropagation();
    tx = -node.x * scale;
    ty = -node.y * scale;
    world.setAttribute("transform", `translate(${tx} ${ty}) scale(${scale})`);
  });
  return group;
}
