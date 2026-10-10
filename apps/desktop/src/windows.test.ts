/**
 * What each window loads, tested without a display.
 *
 * The Gateway window must keep showing the Gateway, and the Router window must
 * show the Router — each from its own origin, so each panel calls only the
 * server that served it.
 */

import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
  gatewayPanelUrl,
  isExternalWebLink,
  isSameOrigin,
  routerPanelUrl,
  routerWindowPreferences,
} from "./windows.ts";

describe("the two windows' URLs", () => {
  it("the Gateway window is the Gateway's origin, exactly as before", () => {
    assert.equal(gatewayPanelUrl(11434), "http://127.0.0.1:11434/");
  });

  it("the Router window is the Router's origin, never the Gateway's", () => {
    assert.equal(routerPanelUrl(11500), "http://127.0.0.1:11500/");
    assert.equal(isSameOrigin(routerPanelUrl(11500), gatewayPanelUrl(11434)), false);
  });
});

describe("the Router window's preferences", () => {
  it("keeps the sandbox, context isolation and no Node", () => {
    const preferences = routerWindowPreferences();
    assert.equal(preferences.contextIsolation, true);
    assert.equal(preferences.sandbox, true);
    assert.equal(preferences.nodeIntegration, false);
  });

  it("has no preload, so the Router page has no bridge to the shell", () => {
    assert.equal("preload" in routerWindowPreferences(), false);
  });
});

describe("keeping the Router window on the Router", () => {
  const router = routerPanelUrl(11500);

  it("admits the Router's own pages", () => {
    assert.equal(isSameOrigin("http://127.0.0.1:11500/#/classifier", router), true);
  });

  it("refuses the Gateway, another port, another host and another scheme", () => {
    for (const url of [
      "http://127.0.0.1:11434/",
      "http://127.0.0.1:11501/",
      "http://localhost:11500/",
      "https://127.0.0.1:11500/",
      "file:///etc/passwd",
      "not a url",
    ]) {
      assert.equal(isSameOrigin(url, router), false, url);
    }
  });

  it("hands only web links to the system browser", () => {
    assert.equal(isExternalWebLink("https://docs.example.com/router"), true);
    assert.equal(isExternalWebLink("http://example.com"), true);
    for (const url of ["file:///etc/passwd", "javascript:alert(1)", "smb://host/share", "nope"]) {
      assert.equal(isExternalWebLink(url), false, url);
    }
  });
});
