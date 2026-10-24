import { providerContract } from "./contract/provider.contract.mjs";
import { createProvider } from "../providers/fake/provider.mjs";
import { memoryStore } from "../core/store.mjs";

const stores = new WeakMap();

providerContract("fake", (config, previous) => {
  const store = previous ? stores.get(previous) : memoryStore();
  const provider = createProvider({ config, store });
  stores.set(provider, store);
  return provider;
});
