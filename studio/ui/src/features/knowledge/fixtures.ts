import type { ConnectionIdentity } from "../../lib/api/client";
import type {
  KnowledgeChunkRecord,
  KnowledgeCitedChunk,
  KnowledgeLibrary,
  KnowledgeSource,
  ListedKnowledgeSource,
} from "../../lib/api/knowledge";

export const testConnection: ConnectionIdentity = {
  epoch: 1,
  origin: "https://rusty.example",
  apiKey: "key",
  tenantFingerprint: "a",
};

export const HASH_A = "a".repeat(64);
export const HASH_B = "b".repeat(64);
export const HASH_C = "c".repeat(64);
export const CHUNK_ADDR_1 = "1".repeat(64);
export const CHUNK_ADDR_2 = "2".repeat(64);

export function listedSource(overrides: Partial<ListedKnowledgeSource> = {}): ListedKnowledgeSource {
  return {
    source_id: "travel-policy",
    scope: { scope: "tenant", id: "acme" },
    kind: "markdown",
    title: "Travel policy",
    author: "human:maya",
    confidence: 0.9,
    created_at: "2026-08-11T00:00:00Z",
    retention: { policy: "pinned" },
    content_hash: HASH_A,
    content_bytes: 4096,
    version: 1,
    chunk_count: 3,
    ...overrides,
  };
}

export function fullSource(overrides: Partial<KnowledgeSource> = {}): KnowledgeSource {
  return {
    source_id: "travel-policy",
    scope: { scope: "tenant", id: "acme" },
    kind: "markdown",
    title: "Travel policy",
    author: "human:maya",
    confidence: 0.9,
    created_at: "2026-08-11T00:00:00Z",
    retention: { policy: "pinned" },
    content_hash: HASH_A,
    body_hash: HASH_C,
    content_bytes: 4096,
    version: 1,
    ...overrides,
  };
}

export function chunkRecord(overrides: Partial<KnowledgeChunkRecord> = {}): KnowledgeChunkRecord {
  return {
    chunk_id: "travel-policy#0",
    source_id: "travel-policy",
    source_hash: HASH_A,
    chunk_index: 0,
    byte_start: 0,
    byte_end: 512,
    content_address: CHUNK_ADDR_1,
    bytes: 512,
    word_count: 84,
    ...overrides,
  };
}

export function citedChunk(overrides: Partial<KnowledgeCitedChunk> = {}): KnowledgeCitedChunk {
  return {
    citation: {
      source_id: "travel-policy",
      source_hash: HASH_A,
      title: "Travel policy",
      chunk_id: "travel-policy#0",
      chunk_index: 0,
      content_address: CHUNK_ADDR_1,
      byte_start: 0,
      byte_end: 512,
    },
    text: "Hotels in Berlin are capped at 140 EUR per night.",
    score: 0.8123,
    word_count: 84,
    ...overrides,
  };
}

export function emptyLibrary(): KnowledgeLibrary {
  return { sources: [], tombstones: [] };
}
