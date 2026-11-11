// Generated per invocation. Contains local capability routes, never OAuth tokens.
export default function (pi) {
  const config = @CONFIG@;
  const routes = new Map(config.routes.map((route) => [route.id, route]));
  const installation = Symbol.for("aegis.relay.pi.installation");
  const identity = config.routes.map((route) => route.endpoint).join("|");

  function clean(headers) {
    return headers && Object.fromEntries(Object.entries(headers).filter(([key]) =>
      !["x-aegis-api-key", "x-aegis-access-token"].includes(key.toLowerCase()),
    ));
  }

  function configuredRoute(baseUrl) {
    if (!baseUrl) return;
    try {
      const url = new URL(baseUrl);
      const parts = url.pathname.split("/");
      const index = parts.indexOf("providers");
      if (index < 0) return;
      const id = decodeURIComponent(parts[index + 1]);
      const route = routes.get(id);
      if (route) return route;
      if (url.origin === config.server) {
        throw new Error(`AGS: provider ${id} is not enabled; run ags configure`);
      }
    } catch (error) {
      if (error instanceof Error && error.message.startsWith("AGS:")) throw error;
    }
  }

  function redirected(original, source, routeFor) {
    const invoke = (method) => (model, context, options) => {
      const route = routeFor(model);
      if (!route) throw new Error(`AGS: ${model.provider} has no enabled relay route; run ags configure`);
      return original[method](
        { ...model, provider: source, baseUrl: route.endpoint, headers: clean(model.headers) },
        context,
        { ...options, headers: clean(options?.headers), transport: "sse" },
      );
    };
    return { ...original, stream: invoke("stream"), streamSimple: invoke("streamSimple") };
  }

  pi.on("session_start", async (_event, ctx) => {
    const registry = ctx.modelRegistry;
    // pi-subagents shares the parent's model runtime, including these providers.
    const runtime = registry.runtime ?? registry;
    if (runtime[installation] !== identity) install(registry);
    runtime[installation] = identity;
    // Start on the configured subscription's alias rather than pi's native provider name. pi
    // accepts the alias once its startup sign-in check has seen it, so retry briefly.
    const current = ctx.model;
    const route = current && routes.get(config.defaults[current.provider]);
    const alias = route && registry.find(route.alias, current.id);
    for (let i = 0; alias && i < 40 && !(await pi.setModel(alias)); i++) {
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  });

  function install(registry) {
    const sources = new Map(config.routes.map((route) => [route.source, registry.getProvider(route.source)]));
    const existingIds = new Set(registry.getAll().map((model) => model.provider));

    for (const id of existingIds) {
      const original = registry.getProvider(id);
      if (!original) continue;
      const fallback = routes.get(config.defaults[id]);
      registry.registerProvider(redirected(original, id, (model) =>
        (model.baseUrl !== original.baseUrl ? configuredRoute(model.baseUrl) : undefined) ?? fallback ?? configuredRoute(original.baseUrl),
      ));
    }

    for (const route of config.routes) {
      const source = sources.get(route.source);
      if (!source) throw new Error(`AGS: pi has no native ${route.source} provider`);
      const models = () => source.getModels().map((model) => ({
        ...model, provider: route.alias, baseUrl: route.endpoint,
      }));
      const aliased = redirected(source, route.source, () => route);
      registry.registerProvider({
        ...aliased,
        id: route.alias,
        name: `AGS · ${route.id}`,
        baseUrl: route.endpoint,
        headers: clean(source.headers),
        getModels: models,
        getAllModels: models,
        refreshModels: undefined,
        // Each alias can hold its own native OAuth sign-in. Only the default
        // subscription falls back to the existing native provider's sign-in.
        auth: {
          ...(source.auth.oauth ? { oauth: {
            ...source.auth.oauth, name: `${source.name} · ${route.id}`,
          } } : {}),
          apiKey: {
            name: `Existing ${source.name} sign-in`,
            async resolve({ signal, credential }) {
              if (credential?.key) {
                return { auth: { apiKey: credential.key, baseUrl: route.endpoint }, source: `Stored ${route.id} credential` };
              }
              if (config.defaults[route.source] !== route.id) return undefined;
              const resolved = typeof registry.getProviderAuth === "function"
                ? await registry.getProviderAuth(route.source, { signal })
                : await registry.runtime.getAuth(route.source, { signal });
              return resolved && {
                ...resolved,
                auth: { ...resolved.auth, baseUrl: route.endpoint, headers: clean(resolved.auth.headers) },
              };
            },
          },
        },
      });
    }
  }
}
