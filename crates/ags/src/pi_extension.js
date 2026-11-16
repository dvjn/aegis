// Written by ags: sends each pi provider through this launch's Aegis proxy, or turns it off.
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { InMemoryCredentialStore, createProvider, lazyStream } from "@earendil-works/pi-ai";
import { builtinProviders } from "@earendil-works/pi-ai/providers/all";
import { ModelRuntime, getAgentDir } from "@earendil-works/pi-coding-agent";

export default async function (pi) {
  const launch = JSON.parse(process.env.AGS_PI_ROUTES || "null");
  const secret = process.env.AGS_LAUNCH_SECRET;
  if (!launch || !secret) return;
  const headers = { "x-ags-launch-secret": secret };
  const builtins = new Map(builtinProviders().map((provider) => [provider.id, provider]));
  // pi's own runtime adds its remote model catalog to each built-in provider.
  const catalog = await ModelRuntime.create({
    credentials: new InMemoryCredentialStore(),
    modelsPath: null,
    allowModelNetwork: false,
    refreshOnCreate: false,
  });
  const readJson = (path) => {
    try {
      return JSON.parse(readFileSync(path, "utf8"));
    } catch {
      return undefined;
    }
  };
  const failing = (message) => (model, context, options) =>
    lazyStream(model, async () => {
      throw new Error(message);
    }, undefined, options?.signal);
  // Listed providers show in pi's model list, so that using one says why it is off.
  const off = (id, name, models, message, listed) =>
    createProvider({
      id,
      name,
      auth: {
        apiKey: {
          name: "Aegis",
          login: async () => {
            throw new Error(message);
          },
          check: async () => (listed ? { type: "api_key", source: "ags" } : undefined),
          resolve: async () => {
            throw new Error(message);
          },
        },
      },
      models,
      api: { stream: failing(message), streamSimple: failing(message) },
    });
  const signIns = new Map();
  // A sign-in stays in its provider's own file, where pi refreshes it under pi's own lock.
  const signIn = (path) => {
    if (!signIns.has(path)) {
      const created = ModelRuntime.create({
        authPath: path,
        modelsPath: null,
        allowModelNetwork: false,
        refreshOnCreate: false,
      });
      signIns.set(path, created);
      created.catch(() => {
        if (signIns.get(path) === created) signIns.delete(path);
      });
    }
    return signIns.get(path);
  };
  const ready = (route) =>
    route.signIn ? readJson(route.credential)?.[route.builtin] !== undefined : existsSync(route.credential);
  const resolve = async (route) => {
    if (!ready(route)) throw new Error(route.missing);
    if (route.signIn) {
      const resolution = await (await signIn(route.credential)).getAuth(route.builtin);
      if (!resolution?.auth.apiKey) throw new Error(route.missing);
      return { auth: { apiKey: resolution.auth.apiKey, headers }, source: "ags" };
    }
    const apiKey = route.key ? readFileSync(route.credential, "utf8").trim() : undefined;
    return { auth: { apiKey, headers }, env: route.env, source: "ags" };
  };
  const providers = [];
  for (const route of launch.routes) {
    const base = catalog.getProvider(route.builtin) ?? builtins.get(route.builtin);
    if (!base) continue;
    const name =
      route.provider === undefined ? base.name : route.id === route.provider ? route.id : `${base.name} (${route.provider})`;
    const pinned = (model) => ({ ...model, provider: route.id, baseUrl: route.baseUrl });
    // pi lists models from filterModels but finds a named one without it, so a built-in name
    // lists nothing yet still serves a default model, `--model` or a saved session.
    const unlisted =
      route.provider === route.id ? {} : { filterModels: () => [], filterAllModels: () => [] };
    if (route.refusal) {
      const refused = off(route.id, name, base.getModels().map(pinned), route.refusal, true);
      providers.push({ ...refused, ...unlisted });
      continue;
    }
    const provider = createProvider({
      id: route.id,
      name,
      baseUrl: route.baseUrl,
      auth: {
        apiKey: {
          name: "Aegis",
          login: async () => {
            throw new Error(ready(route) ? route.again : route.missing);
          },
          check: async () => ({ type: "api_key", source: "ags" }),
          resolve: () => resolve(route),
        },
      },
      models: [],
      api: {
        stream: (model, context, options) => base.stream(pinned(model), context, options),
        streamSimple: (model, context, options) => base.streamSimple(pinned(model), context, options),
      },
    });
    const listed = () => base.getModels().map(pinned);
    providers.push({
      ...provider,
      getModels: listed,
      getAllModels: listed,
      refreshModelsAuth: base.refreshModelsAuth,
      refreshModels: base.refreshModels && ((context) => base.refreshModels(context)),
      ...unlisted,
    });
  }
  const routed = new Set(providers.map((provider) => provider.id));
  const others = new Set(builtins.keys());
  for (const id of Object.keys(readJson(join(getAgentDir(), "models.json"))?.providers ?? {})) others.add(id);
  for (const id of others) {
    if (routed.has(id)) continue;
    const base = builtins.get(id);
    const models = (base?.getModels() ?? []).map((model) => ({ ...model, baseUrl: launch.off.baseUrl }));
    providers.push(off(id, base?.name ?? id, models, launch.off.refusal.replaceAll("{provider}", id), false));
  }
  const register = () => providers.forEach((provider) => pi.registerProvider(provider));
  register();
  // Another extension may have replaced one of these while pi loaded.
  pi.on("session_start", register);
}
