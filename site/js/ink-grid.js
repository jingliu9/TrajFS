(() => {
  const hero = document.querySelector("[data-ink-hero]");
  const canvas = document.querySelector("[data-ink-canvas]");

  if (!hero || !canvas) return;

  const context = canvas.getContext("2d", { alpha: true });
  if (!context) return;
  const gridLayer = document.createElement("canvas");
  const gridContext = gridLayer.getContext("2d", { alpha: true });
  if (!gridContext) return;
  gridLayer.className = "hero-grid-canvas";
  gridLayer.setAttribute("aria-hidden", "true");
  canvas.before(gridLayer);

  const motionQuery = window.matchMedia("(prefers-reduced-motion: reduce)");
  const palette = [
    [33, 37, 49],
    [28, 69, 132],
    [208, 112, 58],
  ];
  const pigmentStrength = [0.82, 1.24, 1.08];

  let width = 0;
  let height = 0;
  let pixelRatio = 1;
  let hexRadius = 42;
  let washTextures = [];
  let cells = [];
  let motifs = [];
  let strokes = [];
  let frame = 0;
  let lastFrameTime = 0;
  let isVisible = true;
  let lastPointer = null;
  let idleTimer = 0;

  const seededRandom = (seed) => {
    const value = Math.sin(seed * 999.91) * 43758.5453;
    return value - Math.floor(value);
  };

  const clamp = (value, minimum, maximum) => Math.max(minimum, Math.min(maximum, value));

  const regionalPigment = (x, y, seed) => {
    const normalizedX = x / Math.max(width, 1);
    const normalizedY = y / Math.max(height, 1);
    const fields = [
      { x: 0.08, y: 0.22, color: 0 },
      { x: 0.42, y: 0.16, color: 1 },
      { x: 0.78, y: 0.3, color: 1 },
      { x: 0.9, y: 0.72, color: 2 },
      { x: 0.18, y: 0.7, color: 2 },
      { x: 0.5, y: 0.82, color: 0 },
    ];
    const driftX = (seededRandom(seed + 31.7) - 0.5) * 0.12;
    const driftY = (seededRandom(seed + 47.3) - 0.5) * 0.12;
    const nearest = fields.reduce((closest, field) => {
      const distance = Math.hypot(
        normalizedX + driftX - field.x,
        normalizedY + driftY - field.y,
      );
      return distance < closest.distance ? { color: field.color, distance } : closest;
    }, { color: 0, distance: Number.POSITIVE_INFINITY });

    if (seededRandom(seed + 63.1) > 0.91) {
      return Math.floor(seededRandom(seed + 79.9) * palette.length);
    }
    return nearest.color;
  };

  const tonedColor = (color, tone) => color.map((channel) => (
    clamp(Math.round(channel * tone), 0, 255)
  ));

  const createWashTextures = () => {
    const variantCount = 6;
    const cssSize = hexRadius * 4;

    washTextures = palette.map((pigment, pigmentIndex) => (
      Array.from({ length: variantCount }, (_, variant) => {
        const texture = document.createElement("canvas");
        texture.width = Math.ceil(cssSize * pixelRatio);
        texture.height = Math.ceil(cssSize * pixelRatio);
        const textureContext = texture.getContext("2d", { alpha: true });
        const seed = 401 + pigmentIndex * 211 + variant * 37;
        const tone = 0.78 + variant / (variantCount - 1) * 0.44;
        const color = tonedColor(pigment, tone);
        const strength = pigmentStrength[pigmentIndex];
        const center = cssSize / 2;
        const bleed = 0.88 + seededRandom(seed + 13.7) * 0.38;
        const edgeDepth = 0.58 + seededRandom(seed + 29.1) * 0.9;
        const textureCell = { x: center, y: center, seed };

        textureContext.setTransform(pixelRatio, 0, 0, pixelRatio, 0, 0);
        const offsetX = (seededRandom(seed + 47.3) - 0.5) * hexRadius * 0.32;
        const offsetY = (seededRandom(seed + 61.9) - 0.5) * hexRadius * 0.32;
        const bloom = textureContext.createRadialGradient(
          center + offsetX,
          center + offsetY,
          hexRadius * 0.12,
          center,
          center,
          hexRadius * 1.38 * bleed,
        );
        bloom.addColorStop(0, `rgba(${color[0]}, ${color[1]}, ${color[2]}, ${strength * 0.022})`);
        bloom.addColorStop(0.58, `rgba(${color[0]}, ${color[1]}, ${color[2]}, ${strength * 0.012})`);
        bloom.addColorStop(1, `rgba(${color[0]}, ${color[1]}, ${color[2]}, 0)`);
        textureContext.fillStyle = bloom;
        textureContext.fillRect(0, 0, cssSize, cssSize);

        for (let layer = 0; layer < 5; layer += 1) {
          const inset = layer * hexRadius * 0.028;
          const layerVariation = 0.78 + seededRandom(seed + layer * 43.7) * 0.44;
          hexPath(textureContext, textureCell, hexRadius * 0.88 - inset, layer);
          textureContext.fillStyle = `rgba(${color[0]}, ${color[1]}, ${color[2]}, ${strength * (0.038 + layer * 0.014) * layerVariation})`;
          textureContext.shadowColor = `rgba(${color[0]}, ${color[1]}, ${color[2]}, ${strength * 0.095})`;
          textureContext.shadowBlur = layer === 0 ? 16 * bleed : 1.5;
          textureContext.fill();
        }

        textureContext.shadowBlur = 0;
        hexPath(textureContext, textureCell, hexRadius * 0.855, 7);
        textureContext.strokeStyle = `rgba(${color[0]}, ${color[1]}, ${color[2]}, ${strength * (0.02 + edgeDepth * 0.038)})`;
        textureContext.lineWidth = 0.45 + edgeDepth * 0.42;
        textureContext.stroke();

        for (let dot = 0; dot < 19; dot += 1) {
          const angle = seededRandom(seed + dot * 3.1) * Math.PI * 2;
          const distance = Math.sqrt(seededRandom(seed + dot * 5.7)) * hexRadius * 0.72;
          const dotSize = 0.3 + seededRandom(seed + dot * 9.2) * 0.95;
          const dotAlpha = 0.07 + seededRandom(seed + dot * 11.8) * 0.11;
          textureContext.fillStyle = `rgba(${color[0]}, ${color[1]}, ${color[2]}, ${strength * dotAlpha})`;
          textureContext.fillRect(
            center + Math.cos(angle) * distance,
            center + Math.sin(angle) * distance,
            dotSize,
            dotSize,
          );
        }

        return { canvas: texture, cssSize };
      })
    ));
  };

  const axialDistance = (firstQ, firstR, secondQ, secondR) => {
    const deltaQ = firstQ - secondQ;
    const deltaR = firstR - secondR;
    return (Math.abs(deltaQ) + Math.abs(deltaR) + Math.abs(deltaQ + deltaR)) / 2;
  };

  const pointToSegmentDistance = (point, start, end) => {
    const segmentX = end.x - start.x;
    const segmentY = end.y - start.y;
    const segmentLength = segmentX * segmentX + segmentY * segmentY;
    if (!segmentLength) return Math.hypot(point.x - start.x, point.y - start.y);

    const projection = Math.max(0, Math.min(1,
      ((point.x - start.x) * segmentX + (point.y - start.y) * segmentY) / segmentLength,
    ));
    const projectedX = start.x + projection * segmentX;
    const projectedY = start.y + projection * segmentY;
    return Math.hypot(point.x - projectedX, point.y - projectedY);
  };

  const hexPath = (targetContext, cell, radius, layer = 0) => {
    targetContext.beginPath();
    for (let point = 0; point < 6; point += 1) {
      const angle = Math.PI / 3 * point - Math.PI / 6;
      const variation = 1 + (seededRandom(cell.seed + point * 7.3 + layer * 19.1) - 0.5) * 0.09;
      const x = cell.x + Math.cos(angle) * radius * variation;
      const y = cell.y + Math.sin(angle) * radius * variation;
      if (point === 0) targetContext.moveTo(x, y);
      else targetContext.lineTo(x, y);
    }
    targetContext.closePath();
  };

  const createGrid = () => {
    const horizontalStep = Math.sqrt(3) * hexRadius;
    const verticalStep = hexRadius * 1.5;
    const columns = Math.ceil(width / horizontalStep) + 3;
    const rows = Math.ceil(height / verticalStep) + 3;
    const nextCells = [];

    for (let row = -1; row < rows; row += 1) {
      for (let column = -1; column < columns; column += 1) {
        const seed = (row + 4) * 131 + (column + 7) * 67;
        const parity = ((row % 2) + 2) % 2;
        const q = column - (row - parity) / 2;
        const x = column * horizontalStep + (parity ? horizontalStep / 2 : 0);
        const y = row * verticalStep;
        const edgeDistance = Math.max(
          Math.abs(x / width - 0.5),
          Math.abs(y / height - 0.5),
        );
        const wash = seededRandom(seed + 4.2);
        const resting = wash > 0.968 || (edgeDistance > 0.43 && wash > 0.91)
          ? 0.045 + seededRandom(seed + 8.8) * 0.1
          : 0;

        nextCells.push({
          x,
          y,
          q,
          r: row,
          seed,
          resting,
          energy: resting,
          target: 0,
          color: regionalPigment(x, y, seed),
          washDepth: 0.68 + seededRandom(seed + 18.6) * 0.82,
          textureVariant: Math.floor(seededRandom(seed + 36.4) * 6),
          motifLinks: [],
        });
      }
    }

    cells = nextCells;
  };

  const createMotifs = () => {
    const specs = width < 680
      ? [
          { x: 0.68, y: 0.36, steps: 4, color: 1, depth: 1.18 },
          { x: 0.24, y: 0.66, steps: 3, color: 2, depth: 0.94 },
        ]
      : [
          { x: 0.76, y: 0.41, steps: 5, color: 1, depth: 1.2 },
          { x: 0.14, y: 0.25, steps: 3, color: 2, depth: 0.92 },
          { x: 0.52, y: 0.68, steps: 4, color: 0, depth: 0.88 },
        ];

    motifs = specs.map((spec, motifIndex) => {
      const desiredX = width * spec.x;
      const desiredY = height * spec.y;
      const center = cells.reduce((nearest, cell) => (
        Math.hypot(cell.x - desiredX, cell.y - desiredY)
          < Math.hypot(nearest.x - desiredX, nearest.y - desiredY) ? cell : nearest
      ));
      const radius = Math.sqrt(3) * hexRadius * spec.steps;
      const vertices = Array.from({ length: 6 }, (_, index) => {
        const angle = index * Math.PI / 3;
        return {
          x: center.x + Math.cos(angle) * radius,
          y: center.y + Math.sin(angle) * radius,
        };
      });
      const starEdges = [
        [0, 2], [2, 4], [4, 0],
        [1, 3], [3, 5], [5, 1],
      ];
      const motif = {
        x: center.x,
        y: center.y,
        q: center.q,
        r: center.r,
        radius,
        steps: spec.steps,
        color: spec.color,
        depth: spec.depth,
        vertices,
        energy: spec.color === 1 ? 0.17 : spec.color === 2 ? 0.15 : 0.11,
        target: 0.34 - motifIndex * 0.04,
      };

      for (const cell of cells) {
        const distance = axialDistance(cell.q, cell.r, center.q, center.r);
        const onOuterRing = distance === spec.steps;
        const onStar = starEdges.some(([start, end]) => (
          pointToSegmentDistance(cell, vertices[start], vertices[end]) < hexRadius * 0.58
        ));
        if (!onOuterRing && !onStar) continue;

        const isCorner = vertices.some((vertex) => (
          Math.hypot(cell.x - vertex.x, cell.y - vertex.y) < hexRadius * 0.7
        ));
        const weight = isCorner ? 0.9 : onOuterRing ? 0.56 : 0.34;
        cell.motifLinks.push({ index: motifIndex, weight });
        cell.color = spec.color;
        cell.washDepth = Math.max(cell.washDepth, spec.depth * (0.72 + weight * 0.35));
        const pigmentResting = spec.color === 1
          ? 0.042 + weight * 0.075
          : spec.color === 2
            ? 0.038 + weight * 0.068
            : 0.025 + weight * 0.052;
        cell.resting = Math.max(cell.resting, pigmentResting);
        cell.energy = Math.max(cell.energy, cell.resting);
      }

      return motif;
    });
  };

  const resize = () => {
    const bounds = hero.getBoundingClientRect();
    width = Math.max(1, Math.round(bounds.width));
    height = Math.max(1, Math.round(bounds.height));
    pixelRatio = Math.min(window.devicePixelRatio || 1, 1.5);
    hexRadius = width < 680 ? 16 : width < 1100 ? 18 : 20;
    canvas.width = Math.round(width * pixelRatio);
    canvas.height = Math.round(height * pixelRatio);
    gridLayer.width = canvas.width;
    gridLayer.height = canvas.height;
    canvas.style.width = `${width}px`;
    canvas.style.height = `${height}px`;
    gridLayer.style.width = `${width}px`;
    gridLayer.style.height = `${height}px`;
    context.setTransform(pixelRatio, 0, 0, pixelRatio, 0, 0);
    gridContext.setTransform(pixelRatio, 0, 0, pixelRatio, 0, 0);
    createWashTextures();
    createGrid();
    createMotifs();
    renderGridLayer();
    seedOpeningWash();
    draw();
  };

  const activate = (x, y, strength = 1, color) => {
    const reach = hexRadius * 4.2;

    for (const cell of cells) {
      const distance = Math.hypot(cell.x - x, cell.y - y);
      if (distance > reach) continue;
      const amount = strength * (1 - distance / reach);
      cell.target = Math.max(cell.target, amount);
      if (color !== undefined && amount > 0.28) cell.color = color;
    }

    for (const motif of motifs) {
      const distance = Math.hypot(motif.x - x, motif.y - y);
      if (distance > motif.radius * 1.22) continue;
      motif.target = Math.max(
        motif.target,
        strength * 0.9 * (1 - distance / (motif.radius * 1.22)),
      );
    }

  };

  const primeWash = (x, y, strength, color, saturation) => {
    activate(x, y, strength, color);
    const reach = hexRadius * 4.2;
    const persistence = [0.25, 0.58, 0.42][color];

    for (const cell of cells) {
      const distance = Math.hypot(cell.x - x, cell.y - y);
      if (distance > reach) continue;
      const amount = strength * (1 - distance / reach);
      cell.energy = Math.max(cell.energy, amount * saturation);
      cell.resting = Math.max(cell.resting, amount * saturation * persistence);
      if (amount > 0.08) cell.color = color;
    }
  };

  const pulseMotif = (motif, strength) => {
    motif.target = Math.max(motif.target, strength);
    for (const cell of cells) {
      const link = cell.motifLinks.find(({ index }) => motifs[index] === motif);
      if (!link) continue;
      cell.target = Math.max(cell.target, strength * link.weight * 0.68);
    }
  };

  const seedOpeningWash = () => {
    const seeds = width < 680
      ? [
          [width * 0.74, height * 0.22, 1, 0.68, 0.56],
          [width * 0.2, height * 0.62, 2, 0.52, 0.5],
          [width * 0.5, height * 0.8, 0, 0.32, 0.28],
        ]
      : [
          [width * 0.12, height * 0.22, 0, 0.34, 0.28],
          [width * 0.36, height * 0.3, 1, 0.6, 0.5],
          [width * 0.78, height * 0.26, 1, 0.74, 0.62],
          [width * 0.9, height * 0.7, 2, 0.58, 0.54],
          [width * 0.48, height * 0.8, 0, 0.36, 0.3],
        ];

    motifs.forEach((motif, index) => pulseMotif(motif, 0.46 - index * 0.05));
    for (const [x, y, color, strength, saturation] of seeds) {
      primeWash(x, y, strength, color, saturation);
    }
  };

  const tracePoints = (points) => {
    context.beginPath();
    context.moveTo(points[0].x, points[0].y);
    for (let index = 1; index < points.length; index += 1) {
      context.lineTo(points[index].x, points[index].y);
    }
    context.closePath();
  };

  const drawMotifs = () => {
    context.save();
    context.globalCompositeOperation = "source-over";

    for (const motif of motifs) {
      const color = palette[motif.color];
      const strength = pigmentStrength[motif.color] * motif.depth;
      const opacity = 0.025 + motif.energy * 0.105 * strength;
      context.strokeStyle = `rgba(${color[0]}, ${color[1]}, ${color[2]}, ${opacity})`;
      context.lineWidth = 0.85;

      tracePoints(motif.vertices);
      context.stroke();
      context.lineWidth = 1.35;
      tracePoints([motif.vertices[0], motif.vertices[2], motif.vertices[4]]);
      context.stroke();
      tracePoints([motif.vertices[1], motif.vertices[3], motif.vertices[5]]);
      context.stroke();
    }

    context.restore();
  };

  const renderGridLayer = () => {
    gridContext.clearRect(0, 0, width, height);
    gridContext.save();
    gridContext.strokeStyle = "rgba(70, 75, 112, 0.052)";
    gridContext.lineWidth = 0.6;

    for (const cell of cells) {
      hexPath(gridContext, cell, hexRadius);
      gridContext.stroke();
    }

    gridContext.restore();
  };

  const drawStrokes = () => {
    context.save();
    context.globalCompositeOperation = "source-over";
    context.lineCap = "round";

    for (const stroke of strokes) {
      const color = palette[stroke.color];
      const strength = pigmentStrength[stroke.color];
      context.beginPath();
      context.moveTo(stroke.fromX, stroke.fromY);
      context.quadraticCurveTo(
        (stroke.fromX + stroke.x) / 2 + Math.sin(stroke.seed) * 7,
        (stroke.fromY + stroke.y) / 2 + Math.cos(stroke.seed) * 7,
        stroke.x,
        stroke.y,
      );
      context.strokeStyle = `rgba(${color[0]}, ${color[1]}, ${color[2]}, ${0.052 * stroke.life * strength})`;
      context.lineWidth = hexRadius * 0.7 * stroke.life;
      context.shadowColor = `rgba(${color[0]}, ${color[1]}, ${color[2]}, ${0.1 * strength})`;
      context.shadowBlur = 20;
      context.stroke();
    }

    context.restore();
  };

  const drawInkCell = (cell) => {
    const motifIntensity = cell.motifLinks.reduce((strongest, link) => (
      Math.max(strongest, motifs[link.index].energy * link.weight)
    ), 0);
    const intensity = Math.max(cell.resting, cell.energy, motifIntensity) * cell.washDepth;
    if (intensity < 0.015) return;

    const texture = washTextures[cell.color][cell.textureVariant];
    context.save();
    context.globalCompositeOperation = "source-over";
    context.globalAlpha = clamp(intensity, 0, 1);
    context.drawImage(
      texture.canvas,
      cell.x - texture.cssSize / 2,
      cell.y - texture.cssSize / 2,
      texture.cssSize,
      texture.cssSize,
    );
    context.restore();
  };

  const draw = () => {
    context.clearRect(0, 0, width, height);
    drawStrokes();
    for (const cell of cells) drawInkCell(cell);
    drawMotifs();
  };

  const update = () => {
    for (const motif of motifs) {
      motif.energy += (motif.target - motif.energy) * 0.055;
      motif.target *= 0.982;
      if (motif.target < 0.008) motif.target = 0;
    }

    for (const cell of cells) {
      cell.energy += (cell.target - cell.energy) * 0.075;
      cell.target *= 0.986;
      if (cell.target < 0.004) cell.target = 0;
    }

    strokes = strokes.filter((stroke) => {
      stroke.life *= 0.965;
      return stroke.life > 0.025;
    });
  };

  const animate = (time) => {
    if (!isVisible || motionQuery.matches) {
      frame = 0;
      return;
    }

    if (time - lastFrameTime >= 1000 / 30) {
      update();
      draw();
      lastFrameTime = time;
    }
    frame = window.requestAnimationFrame(animate);
  };

  const start = () => {
    if (!frame && isVisible && !motionQuery.matches) {
      frame = window.requestAnimationFrame(animate);
    }
  };

  const onPointerMove = (event) => {
    if (motionQuery.matches || event.pointerType === "touch") return;
    const bounds = hero.getBoundingClientRect();
    const point = { x: event.clientX - bounds.left, y: event.clientY - bounds.top };
    const color = point.x / width > 0.68 ? 2 : point.y / height < 0.48 ? 1 : 0;

    activate(point.x, point.y, 0.95, color);
    if (lastPointer) {
      strokes.push({
        fromX: lastPointer.x,
        fromY: lastPointer.y,
        x: point.x,
        y: point.y,
        color,
        life: 1,
        seed: point.x * 0.017 + point.y * 0.013,
      });
      if (strokes.length > 70) strokes.shift();
    }
    lastPointer = point;
    start();
  };

  const onPointerDown = (event) => {
    const bounds = hero.getBoundingClientRect();
    const x = event.clientX - bounds.left;
    const y = event.clientY - bounds.top;
    const color = Math.floor((x / Math.max(width, 1)) * palette.length) % palette.length;
    activate(x, y, 1.35, color);
    start();
  };

  hero.addEventListener("pointermove", onPointerMove, { passive: true });
  hero.addEventListener("pointerdown", onPointerDown, { passive: true });
  hero.addEventListener("pointerleave", () => { lastPointer = null; });

  const visibilityObserver = new IntersectionObserver(([entry]) => {
    isVisible = entry.isIntersecting;
    if (isVisible) start();
    else if (frame) {
      window.cancelAnimationFrame(frame);
      frame = 0;
    }
  });

  visibilityObserver.observe(hero);

  const resizeObserver = new ResizeObserver(resize);
  resizeObserver.observe(hero);

  motionQuery.addEventListener("change", () => {
    seedOpeningWash();
    draw();
    start();
  });

  idleTimer = window.setInterval(() => {
    if (!isVisible || motionQuery.matches || document.hidden) return;
    const seed = Date.now() * 0.0001;
    const motif = motifs[Math.floor(seededRandom(seed) * motifs.length)];
    pulseMotif(motif, 0.38 + seededRandom(seed + 4.1) * 0.2);
    activate(
      width * (0.08 + seededRandom(seed + 8.2) * 0.84),
      height * (0.08 + seededRandom(seed + 12.3) * 0.78),
      0.24,
      Math.floor(seededRandom(seed + 16.4) * palette.length),
    );
    start();
  }, 1800);

  window.addEventListener("pagehide", () => window.clearInterval(idleTimer), { once: true });
  resize();
  start();
})();