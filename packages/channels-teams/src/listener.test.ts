import { afterEach, describe, expect, it, vi } from "vitest";
import express from "express";
import type { Request, Response, NextFunction } from "express";
import type { CloudAdapter } from "@microsoft/agents-hosting";
import { createTeamsServer } from "./listener.js";

type Handler = (req: Request, res: Response, next: NextFunction) => unknown;

function fixture() {
  const post = vi.spyOn(express.application, "post");
  const process = vi.fn(async () => {});
  createTeamsServer({
    adapter: { process } as unknown as CloudAdapter,
    authConfig: { clientId: "test-app", tenantId: "test-tenant" },
    port: 0,
    onTurnContext: async () => {},
  });
  const route = post.mock.calls.find(
    (args) => args[0] === "/api/messages",
  ) as unknown as [string, Handler, Handler];
  const authorize = route[1] as Handler;
  const handle = route[2] as Handler;
  const res = {
    headersSent: false,
    status: vi.fn().mockReturnThis(),
    send: vi.fn().mockReturnThis(),
    end: vi.fn(),
  };
  return {
    authorize,
    handle,
    process,
    res,
    response: res as unknown as Response,
  };
}

afterEach(() => vi.restoreAllMocks());

describe("Teams SDK authentication boundary (no listening socket)", () => {
  it("rejects a callerId-only request before CloudAdapter.process", async () => {
    const { authorize, process, res, response } = fixture();
    const next = vi.fn();
    authorize(
      {
        method: "POST",
        headers: {},
        body: { type: "invoke", callerId: "urn:botframework:azure" },
      } as Request,
      response,
      next,
    );
    await vi.waitFor(() => expect(res.status).toHaveBeenCalledWith(401));
    expect(next).not.toHaveBeenCalled();
    expect(process).not.toHaveBeenCalled();
  });

  it("rejects a malformed bearer token without contacting a live identity service", async () => {
    const { authorize, process, res, response } = fixture();
    const next = vi.fn();
    authorize(
      {
        method: "POST",
        headers: { authorization: "Bearer not-a-jwt" },
        body: { type: "invoke" },
      } as Request,
      response,
      next,
    );
    await vi.waitFor(() => expect(res.status).toHaveBeenCalledWith(401));
    expect(next).not.toHaveBeenCalled();
    expect(process).not.toHaveBeenCalled();
  });

  it("rejects a serviceUrl different from verified middleware claims", () => {
    const { handle, process, res, response } = fixture();
    // Synthetic post-authentication state, not evidence of live authentication.
    const req = {
      user: { serviceurl: "https://connector.example.invalid" },
      body: { type: "invoke", serviceUrl: "https://other.example.invalid" },
    } as unknown as Request;
    handle(req, response, vi.fn());
    expect(res.status).toHaveBeenCalledWith(403);
    expect(process).not.toHaveBeenCalled();
  });

  it("strips wire callerId and forwards the authenticated request to the SDK", () => {
    const { handle, process, response } = fixture();
    const req = {
      user: {
        aud: "test-app",
        serviceurl: "https://connector.example.invalid",
      },
      body: {
        type: "invoke",
        serviceUrl: "https://connector.example.invalid",
        callerId: "spoofed",
        unknownField: 1,
      },
    } as unknown as Request;
    handle(req, response, vi.fn());
    expect(req.body.callerId).toBeUndefined();
    expect(req.body.unknownField).toBe(1);
    expect(process).toHaveBeenCalledWith(req, response, expect.any(Function));
  });
});
