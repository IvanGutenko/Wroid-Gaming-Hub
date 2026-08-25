(function exposeWroidProfileModel(root, factory) {
  "use strict";

  const model = factory();
  if (typeof module === "object" && module.exports) module.exports = model;
  root.WroidProfileModel = model;
})(typeof globalThis === "object" ? globalThis : this, () => {
  "use strict";

  const supportedKeys = new Set([
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9",
    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m",
    "n", "o", "p", "q", "r", "s", "t", "u", "v", "w", "x", "y", "z",
    "space", "tab", "shift", "ctrl", "alt", "up", "left", "down", "right", "esc",
  ]);
  const supportedButtons = new Set(["left", "right", "middle", "side", "extra"]);
  const mouseButtonLabels = Object.freeze({
    left: "M1",
    right: "M2",
    middle: "M3",
    side: "M4",
    extra: "M5",
  });

  const canonicalKey = (value) => String(value ?? "").trim().toLowerCase();
  const clone = (value) => JSON.parse(JSON.stringify(value));

  function normalizeProfile(source) {
    const profile = clone(source);
    profile.orientation ||= "landscape";
    profile.layers ||= [];
    profile.bindings ||= [];
    return profile;
  }

  function layerName(binding) {
    if (binding?.input?.kind === "mouse_move") return "base";
    return binding?.layer ?? "base";
  }

  function layerDisplayName(binding) {
    if (binding?.input?.kind === "mouse_move") return "Base";
    return binding?.layer ?? "Base";
  }

  function clearMouseMoveRouting(binding) {
    if (binding?.input?.kind !== "mouse_move") return false;
    const changed = binding.layer != null || binding.modifier != null;
    delete binding.layer;
    delete binding.modifier;
    return changed;
  }

  function inputKeys(input) {
    if (input?.kind === "key") return [input.key];
    if (input?.kind === "key_cluster") return [input.up, input.left, input.down, input.right];
    return [];
  }

  function scopedInputs(input) {
    if (input?.kind === "key") return [{ namespace: "key", value: input.key }];
    if (input?.kind === "key_cluster") {
      return [input.up, input.left, input.down, input.right]
        .map((value) => ({ namespace: "key", value }));
    }
    if (input?.kind === "mouse_button") {
      return [{ namespace: "mouse_button", value: input.button }];
    }
    return [];
  }

  function baseControlKey(binding) {
    const input = binding?.input || {};
    if (input.kind === "key") return canonicalKey(input.key).toUpperCase();
    if (input.kind === "key_cluster") {
      const keys = [input.up, input.left, input.down, input.right].map(canonicalKey);
      if (keys.join("") === "wasd") return "WASD";
      if (keys.join(",") === "up,left,down,right") return "ARROWS";
      return "4 KEY";
    }
    if (input.kind === "mouse_button") {
      return mouseButtonLabels[canonicalKey(input.button)] || "M?";
    }
    return "MOUSE";
  }

  function controlKey(binding) {
    const base = baseControlKey(binding);
    const modifier = canonicalKey(binding?.modifier);
    return modifier ? `${modifier.toUpperCase()}+${base}` : base;
  }

  function inputSummary(binding) {
    const input = binding?.input;
    const chord = controlKey(binding);
    if (!input) return "NO INPUT";
    if (input.kind === "key") return `KEY / ${chord || "—"}`;
    if (input.kind === "key_cluster") return `CLUSTER / ${chord}`;
    if (input.kind === "mouse_button") return `MOUSE / ${chord}`;
    if (input.kind === "mouse_move") return "MOUSE / RELATIVE · ALWAYS LIVE";
    return String(input.kind || "NO INPUT").toUpperCase();
  }

  function renameLayer(profile, previousName, nextName) {
    const target = profile.layers.find((entry) => entry.name === previousName);
    if (!target) return 0;
    target.name = nextName;
    let updated = 0;
    profile.bindings.forEach((binding) => {
      if (binding.layer === previousName) {
        binding.layer = nextName;
        updated += 1;
      }
    });
    return updated;
  }

  function layerRenameError(profile, previousName, nextName) {
    const candidate = String(nextName ?? "").trim();
    if (!candidate) return "Layer name must not be empty.";
    if (candidate.toLowerCase() === "base") return "Layer name Base is reserved.";
    if (profile.layers.some((layer) => layer.name !== previousName && layer.name.trim() === candidate)) {
      return `Layer ${candidate} already exists.`;
    }
    return null;
  }

  function deleteLayer(profile, name) {
    const index = profile.layers.findIndex((entry) => entry.name === name);
    if (index < 0) return 0;
    profile.layers.splice(index, 1);
    let moved = 0;
    profile.bindings.forEach((binding) => {
      if (binding.layer === name) {
        delete binding.layer;
        moved += 1;
      }
    });
    return moved;
  }

  function finiteWithin(value, minimum, maximum) {
    return Number.isFinite(value) && value >= minimum - 1e-9 && value <= maximum + 1e-9;
  }

  function validatePoint(value, label, errors) {
    if (!value || !finiteWithin(value.x, 0, 1) || !finiteWithin(value.y, 0, 1)) {
      errors.push(`${label} must use normalized x/y coordinates within 0.0..=1.0`);
    }
  }

  function validateAction(action, bindingName, errors) {
    if (!action) return;
    if (action.kind === "tap" || action.kind === "hold") {
      validatePoint(action.point, `binding ${bindingName} ${action.kind} point`, errors);
      return;
    }
    if (action.kind === "virtual_joystick") {
      validatePoint(action.center, `binding ${bindingName} virtual_joystick center`, errors);
      if (!Number.isFinite(action.radius) || action.radius <= 0 || action.radius > 1) {
        errors.push(`binding ${bindingName} virtual_joystick radius must be finite and within 0.0..=1.0`);
      }
      const deadZone = action.dead_zone ?? 0;
      if (!Number.isFinite(deadZone) || deadZone < 0 || deadZone >= 1) {
        errors.push(`binding ${bindingName} virtual_joystick dead_zone must be finite and within 0.0..1.0`);
      } else if (Number.isFinite(action.radius) && deadZone >= action.radius) {
        errors.push(`binding ${bindingName} virtual_joystick dead_zone must be smaller than radius`);
      }
      if (action.reaffirm_ms === 0) {
        errors.push(`binding ${bindingName} virtual_joystick reaffirm_ms must be greater than zero`);
      }
      return;
    }
    if (action.kind === "mouse_aim") {
      const region = action.region;
      if (
        !region
        || !finiteWithin(region.x, 0, 1)
        || !finiteWithin(region.y, 0, 1)
        || !Number.isFinite(region.w)
        || !Number.isFinite(region.h)
        || region.w <= 0
        || region.h <= 0
        || region.x + region.w > 1.000000001
        || region.y + region.h > 1.000000001
      ) {
        errors.push(`binding ${bindingName} mouse_aim region must stay inside the normalized viewport with positive w/h`);
      }
      if (!Number.isFinite(action.sensitivity) || action.sensitivity <= 0) {
        errors.push(`binding ${bindingName} mouse_aim sensitivity must be finite and greater than zero`);
      }
      if (action.toggle_key && !supportedKeys.has(canonicalKey(action.toggle_key))) {
        errors.push(`binding ${bindingName} mouse_aim toggle_key must be a supported key name`);
      }
      const threshold = action.recenter_threshold ?? 0.7;
      if (!Number.isFinite(threshold) || threshold < 0.1 || threshold > 1) {
        errors.push(`binding ${bindingName} mouse_aim recenter_threshold must be finite and within 0.1..=1.0`);
      }
      if (action.ads_multiplier != null && (!Number.isFinite(action.ads_multiplier) || action.ads_multiplier < 0.1 || action.ads_multiplier > 1)) {
        errors.push(`binding ${bindingName} mouse_aim ads_multiplier must be finite and within 0.1..=1.0`);
      }
      if (action.reaffirm_ms === 0) {
        errors.push(`binding ${bindingName} mouse_aim reaffirm_ms must be greater than zero`);
      }
      return;
    }
    if (action.kind === "radial_select") {
      validatePoint(action.center, `binding ${bindingName} radial_select center`, errors);
      const distance = action.distance ?? 0.07;
      if (!Number.isFinite(distance) || distance <= 0 || distance > 1) {
        errors.push(`binding ${bindingName} radial_select distance must be finite and within 0.0..=1.0`);
      }
      if (action.direction && !["up", "down", "left", "right"].includes(action.direction)) {
        errors.push(`binding ${bindingName} radial_select direction must be one of up/down/left/right`);
      }
      return;
    }
    if (action.kind === "macro") {
      if (!action.steps?.length) errors.push(`binding ${bindingName} macro must contain at least one step`);
      (action.steps || []).forEach((step, index) => validateAction(step, `${bindingName}.step[${index}]`, errors));
    }
  }

  function validateInput(input, bindingName, errors) {
    if (input?.kind === "key") {
      if (!String(input.key ?? "").trim()) errors.push(`binding ${bindingName} has an empty key input`);
      else if (!supportedKeys.has(canonicalKey(input.key))) errors.push(`binding ${bindingName} uses unsupported key input: ${input.key}`);
    } else if (input?.kind === "key_cluster") {
      const keys = [input.up, input.left, input.down, input.right];
      if (keys.some((key) => !String(key ?? "").trim())) {
        errors.push(`binding ${bindingName} has an empty key_cluster input`);
      } else if (keys.some((key) => !supportedKeys.has(canonicalKey(key)))) {
        errors.push(`binding ${bindingName} key_cluster contains an unsupported key`);
      }
    } else if (input?.kind === "mouse_button") {
      if (!String(input.button ?? "").trim()) errors.push(`binding ${bindingName} has an empty mouse button`);
      else if (!supportedButtons.has(canonicalKey(input.button))) errors.push(`binding ${bindingName} uses unsupported mouse button: ${input.button}`);
    }
  }

  function validateCompatibility(input, action, bindingName, errors) {
    if (!input || !action) return;
    let required = null;
    if (["tap", "hold", "radial_select"].includes(action.kind) && !["key", "mouse_button"].includes(input.kind)) required = "key or mouse_button";
    if (action.kind === "virtual_joystick" && input.kind !== "key_cluster") required = "key_cluster";
    if (action.kind === "mouse_aim" && input.kind !== "mouse_move") required = "mouse_move";
    if (required) {
      errors.push(`binding ${bindingName} pairs ${input.kind} input with ${action.kind} action; ${action.kind} requires ${required}`);
    }
  }

  function validateProfile(profile) {
    const errors = [];
    if (profile.schema_version !== 2) errors.push(`schema_version must be 2, got ${profile.schema_version}`);
    if (!String(profile.name ?? "").trim()) errors.push("name must not be empty");
    if (!String(profile.package_name ?? "").trim()) errors.push("package_name must not be empty");

    const layers = profile.layers || [];
    const layerNames = new Set();
    const declaredLayers = new Set();
    layers.forEach((layer) => {
      const name = String(layer.name ?? "").trim();
      if (!name) errors.push("layer name must not be empty");
      else if (layerNames.has(name)) errors.push(`duplicate layer name: ${layer.name}`);
      layerNames.add(name);
      if (name.toLowerCase() === "base") errors.push("layer name base is reserved");
      declaredLayers.add(layer.name);
    });
    if (layers.length > 64) errors.push("profile may declare at most 64 layers");

    const activationKeys = new Set();
    const layerActivationKeys = new Set();
    layers.forEach((layer) => {
      const key = layer.activation?.key ?? "";
      const canonical = canonicalKey(key);
      if (!supportedKeys.has(canonical)) {
        errors.push(`layer ${layer.name} uses unsupported activation key: ${key}`);
      }
      if (activationKeys.has(canonical)) errors.push(`duplicate layer activation key: ${key}`);
      activationKeys.add(canonical);
      layerActivationKeys.add(`${layer.name}\u0000${canonical}`);
    });

    (profile.bindings || []).forEach((binding) => {
      if (binding.layer == null && binding.modifier == null) {
        inputKeys(binding.input).forEach((key) => {
          if (activationKeys.has(canonicalKey(key))) {
            errors.push(`layer activation key ${key} cannot be used by a base-layer binding`);
          }
        });
      }
    });

    const bindingNames = new Set();
    const scopedInputKeys = new Set();
    (profile.bindings || []).forEach((binding) => {
      const bindingName = String(binding.name ?? "").trim();
      if (!bindingName) errors.push("binding name must not be empty");
      else if (bindingNames.has(bindingName)) errors.push(`duplicate binding name: ${binding.name}`);
      bindingNames.add(bindingName);

      validateInput(binding.input, binding.name, errors);
      validateAction(binding.action, binding.name, errors);
      validateCompatibility(binding.input, binding.action, binding.name, errors);

      if (binding.layer != null && !declaredLayers.has(binding.layer)) {
        errors.push(`binding ${binding.name} references unknown layer: ${binding.layer}`);
      }

      if (binding.modifier != null) {
        const modifier = canonicalKey(binding.modifier);
        if (!supportedKeys.has(modifier)) {
          errors.push(`binding ${binding.name} uses unsupported modifier: ${binding.modifier}`);
        }
        if (binding.input?.kind === "key" && canonicalKey(binding.input.key) === modifier) {
          errors.push(`binding ${binding.name} modifier must differ from input key: ${binding.input.key}`);
        }
        if (binding.input?.kind === "key_cluster") {
          inputKeys(binding.input).forEach((key) => {
            if (canonicalKey(key) === modifier) {
              errors.push(`binding ${binding.name} modifier must differ from key_cluster key: ${key}`);
            }
          });
        }
        if (binding.input?.kind === "mouse_move") {
          errors.push(`binding ${binding.name} cannot use a modifier with mouse_move input`);
        }
        if (modifier === "ctrl") {
          inputKeys(binding.input).forEach((key) => {
            const canonical = canonicalKey(key);
            if (canonical === "esc" || canonical === "c") {
              errors.push(`binding ${binding.name} uses ctrl+${canonical}, which is reserved for the session exit hotkey`);
            }
          });
        }
      }

      const scopeLayer = binding.layer ?? "base";
      const scopeModifier = binding.modifier == null ? null : canonicalKey(binding.modifier);
      const localInputs = new Set();
      scopedInputs(binding.input).forEach(({ namespace, value }) => {
        const canonical = canonicalKey(value);
        const localKey = `${namespace}\u0000${canonical}`;
        if (localInputs.has(localKey)) return;
        localInputs.add(localKey);
        const modifierScopeKey = scopeModifier == null ? "<none>" : `<some>${scopeModifier}`;
        const scopeKey = `${scopeLayer}\u0000${modifierScopeKey}\u0000${namespace}\u0000${canonical}`;
        if (scopedInputKeys.has(scopeKey)) {
          const modifier = scopeModifier == null ? "without a modifier" : `with modifier ${scopeModifier}`;
          errors.push(`${namespace} ${value} drives multiple bindings in ${scopeLayer} layer ${modifier}`);
        }
        scopedInputKeys.add(scopeKey);
        if (
          namespace === "key"
          && binding.layer != null
          && layerActivationKeys.has(`${scopeLayer}\u0000${canonical}`)
        ) {
          errors.push(`layer activation key ${value} cannot be used by binding ${binding.name} inside layer ${scopeLayer}`);
        }
      });
    });

    return errors;
  }

  function createPreviewState() {
    return {
      pressedKeys: new Set(),
      pressedButtons: new Set(),
      activeLayers: new Set(),
      toggledLayers: new Set(),
      latchedTapIndexes: new Set(),
      mouseMoving: false,
    };
  }

  function refreshActiveLayers(preview, profile) {
    preview.activeLayers.clear();
    (profile.layers || []).forEach((layer) => {
      const key = canonicalKey(layer.activation?.key);
      if (layer.activation?.kind === "hold" && preview.pressedKeys.has(key)) {
        preview.activeLayers.add(layer.name);
      }
      if (layer.activation?.kind === "toggle" && preview.toggledLayers.has(layer.name)) {
        preview.activeLayers.add(layer.name);
      }
    });
  }

  function setPreviewKey(preview, profile, key, pressed) {
    const canonical = canonicalKey(key);
    const wasPressed = preview.pressedKeys.has(canonical);
    if (pressed && !wasPressed) {
      (profile.layers || []).forEach((layer) => {
        if (layer.activation?.kind !== "toggle" || canonicalKey(layer.activation.key) !== canonical) return;
        if (preview.toggledLayers.has(layer.name)) preview.toggledLayers.delete(layer.name);
        else preview.toggledLayers.add(layer.name);
      });
    }
    if (pressed) preview.pressedKeys.add(canonical);
    else preview.pressedKeys.delete(canonical);
    refreshActiveLayers(preview, profile);
    const activationKey = (profile.layers || [])
      .some((layer) => canonicalKey(layer.activation?.key) === canonical);
    if (pressed && !wasPressed && !activationKey) {
      latchTapBindingsForSource(preview, profile, "key", canonical);
    }
    reconcileTapLatches(preview, profile);
  }

  function setPreviewButton(preview, profile, button, pressed) {
    const canonical = canonicalKey(button);
    const wasPressed = preview.pressedButtons.has(canonical);
    if (pressed) preview.pressedButtons.add(canonical);
    else preview.pressedButtons.delete(canonical);
    if (pressed && !wasPressed) {
      latchTapBindingsForSource(preview, profile, "mouse_button", canonical);
    }
    reconcileTapLatches(preview, profile);
  }

  function bindingLayerIndex(profile, binding) {
    if (!binding.layer) return 0;
    const index = (profile.layers || []).findIndex((layer) => layer.name === binding.layer);
    return index < 0 ? -1 : index + 1;
  }

  function bindingHasSource(binding, namespace, value) {
    return scopedInputs(binding.input).some((source) => (
      source.namespace === namespace && canonicalKey(source.value) === value
    ));
  }

  function selectedLayerIndex(profile, preview, namespace, value) {
    let selected = 0;
    (profile.bindings || []).forEach((binding) => {
      if (!bindingHasSource(binding, namespace, value)) return;
      const index = bindingLayerIndex(profile, binding);
      if (index > selected && preview.activeLayers.has(binding.layer)) selected = index;
    });
    return selected;
  }

  function bindingAvailableForSource(profile, preview, binding, namespace, value) {
    const ownLayer = bindingLayerIndex(profile, binding);
    if (ownLayer < 0 || ownLayer !== selectedLayerIndex(profile, preview, namespace, value)) return false;
    const modifier = canonicalKey(binding.modifier);
    if (modifier && !preview.pressedKeys.has(modifier)) return false;
    if (!modifier) {
      const hasHeldSibling = (profile.bindings || []).some((sibling) => (
        sibling !== binding
        && bindingLayerIndex(profile, sibling) === ownLayer
        && sibling.modifier
        && preview.pressedKeys.has(canonicalKey(sibling.modifier))
        && bindingHasSource(sibling, namespace, value)
      ));
      if (hasHeldSibling) return false;
    }
    return true;
  }

  function latchTapBindingsForSource(preview, profile, namespace, value) {
    (profile.bindings || []).forEach((binding, index) => {
      if (
        binding.action?.kind === "tap"
        && bindingHasSource(binding, namespace, value)
        && bindingAvailableForSource(profile, preview, binding, namespace, value)
      ) {
        preview.latchedTapIndexes.add(index);
      }
    });
  }

  function reconcileTapLatches(preview, profile) {
    preview.latchedTapIndexes.forEach((index) => {
      const binding = profile.bindings?.[index];
      if (binding?.action?.kind !== "tap") {
        preview.latchedTapIndexes.delete(index);
        return;
      }
      const available = scopedInputs(binding.input).some(({ namespace, value }) => {
        const canonical = canonicalKey(value);
        const held = namespace === "key"
          ? preview.pressedKeys.has(canonical)
          : preview.pressedButtons.has(canonical);
        return held && bindingAvailableForSource(profile, preview, binding, namespace, canonical);
      });
      if (!available) preview.latchedTapIndexes.delete(index);
    });
  }

  function reconcileSelectedBinding(profile, selectedLayer, selectedIndex) {
    const editingLayer = selectedLayer === null ? "base" : selectedLayer;
    const selected = profile.bindings?.[selectedIndex];
    if (selected && layerName(selected) === editingLayer) return selectedIndex;
    return (profile.bindings || []).findIndex((binding) => layerName(binding) === editingLayer);
  }

  function activePreviewBindingIndexes(profile, preview) {
    const result = [];
    const activationKeys = new Set((profile.layers || []).map((layer) => canonicalKey(layer.activation?.key)));
    (profile.bindings || []).forEach((binding, index) => {
      if (binding.action?.kind === "tap") {
        if (preview.latchedTapIndexes.has(index)) result.push(index);
        return;
      }
      if (binding.input?.kind === "mouse_move") {
        if (preview.mouseMoving || (binding.action?.toggle_key && preview.pressedKeys.has(canonicalKey(binding.action.toggle_key)))) {
          result.push(index);
        }
        return;
      }
      const active = scopedInputs(binding.input).some(({ namespace, value }) => {
        const canonical = canonicalKey(value);
        if (namespace === "key" && activationKeys.has(canonical)) return false;
        const held = namespace === "key"
          ? preview.pressedKeys.has(canonical)
          : preview.pressedButtons.has(canonical);
        return held && bindingAvailableForSource(profile, preview, binding, namespace, canonical);
      });
      if (active) result.push(index);
    });
    return result;
  }

  return Object.freeze({
    supportedKeys,
    supportedButtons,
    normalizeProfile,
    layerName,
    layerDisplayName,
    clearMouseMoveRouting,
    renameLayer,
    layerRenameError,
    deleteLayer,
    controlKey,
    inputSummary,
    validateProfile,
    createPreviewState,
    setPreviewKey,
    setPreviewButton,
    activePreviewBindingIndexes,
    reconcileSelectedBinding,
  });
});
