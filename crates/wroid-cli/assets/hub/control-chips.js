"use strict";

(function installControlChips(root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) {
    module.exports = api;
  } else {
    root.WroidHubControlChips = api;
  }
})(typeof globalThis === "object" ? globalThis : this, function createControlChips() {
  function controlChipLabels(game) {
    const labels = [
      `${game.controls.layers} layers`,
      `${game.controls.taps} taps`,
      `${game.controls.holds} holds`,
      `${game.controls.joysticks} sticks`,
      game.controls.mouseAim ? "mouse aim" : "no mouse aim",
    ];
    if (game.controls.radialSelects > 0) {
      labels.push(`${game.controls.radialSelects} wheel`);
    }
    return labels;
  }

  const quickStarts = Object.freeze({
    standoff: Object.freeze([
      "WASD move", "Mouse aim", "LMB fire", "RMB aim", "R reload",
      "Space jump", "C crouch", "1/2 weapons", "F action",
    ]),
    pubg: Object.freeze([
      "WASD move", "Mouse aim", "LMB fire", "RMB aim", "R reload",
      "Space jump", "C crouch", "Z prone", "Q/E lean", "F loot", "M map",
    ]),
    freefire: Object.freeze([
      "WASD move", "Mouse aim", "LMB fire", "RMB scope", "R reload",
      "Space jump", "C crouch", "Z prone", "F interact",
    ]),
    brawl: Object.freeze([
      "WASD move", "Arrow keys attack", "Space super", "E gadget",
    ]),
  });

  function controlQuickStart(game) {
    const mouseAim = game.kind !== "brawl";
    const primary = quickStarts[game.kind]
      ? [...quickStarts[game.kind]]
      : [`${game.bindings || 0} mapped controls`];
    return {
      primary,
      safety: [
        ...(mouseAim ? ["Tab — toggle mouse aim"] : []),
        "F12 — release input",
        "Ctrl+Esc — stop game",
      ],
    };
  }

  function editorActionFor(game) {
    return game.installed !== false && !game.calibration?.ready ? "calibrate" : "edit";
  }

  return { controlChipLabels, controlQuickStart, editorActionFor };
});
