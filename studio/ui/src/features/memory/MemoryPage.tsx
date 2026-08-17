import { useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { StudioApiError } from "../../lib/api/client";
import { listMemoryConflicts, queryMemory, type MemoryQueryInput, type MemoryRecord } from "../../lib/api/memory";
import { PageHeader } from "../../components/PageHeader";
import { ConflictInbox } from "./ConflictInbox";
import { CorrectionPanel } from "./CorrectionPanel";
import { CreateMemoryPanel } from "./CreateMemoryPanel";
import { LedgerView } from "./LedgerView";
import { MemoryDetail } from "./MemoryDetail";
import styles from "./MemoryPage.module.css";

type MemoryTab = "ledger" | "create" | "correct" | "conflicts";

interface SearchState {
  query: MemoryQueryInput;
  records: MemoryRecord[];
  searchedAt: Date;
}

export function MemoryPage() {
  const queryClient = useQueryClient();
  const [tab, setTab] = useState<MemoryTab>("ledger");
  const [search, setSearch] = useState<SearchState | null>(null);
  const [searchError, setSearchError] = useState("");
  const [searching, setSearching] = useState(false);
  const [selectedId, setSelectedId] = useState("");
  const [correctionTargetId, setCorrectionTargetId] = useState("");

  const conflicts = useQuery({
    queryKey: ["memory-conflicts"],
    queryFn: () => listMemoryConflicts(),
  });

  const conflictedIds = useMemo(
    () => new Set((conflicts.data ?? []).flatMap((conflict) => conflict.memory_ids)),
    [conflicts.data],
  );

  async function runSearch(query: MemoryQueryInput) {
    setSearching(true);
    setSearchError("");
    try {
      const records = await queryMemory(query);
      setSearch({ query, records, searchedAt: new Date() });
      setSelectedId("");
    } catch (caught) {
      setSearch(null);
      setSearchError(caught instanceof StudioApiError ? caught.message : "The memory query could not be completed.");
    } finally {
      setSearching(false);
    }
  }

  function refreshEvidence() {
    void conflicts.refetch();
    if (search) void runSearch(search.query);
  }

  function inspectRecord(memoryId: string) {
    setSelectedId(memoryId);
    setTab("ledger");
  }

  function correctRecord(memoryId: string) {
    setCorrectionTargetId(memoryId);
    setTab("correct");
  }

  function afterMutation() {
    void queryClient.invalidateQueries({ queryKey: ["memory-conflicts"] });
    if (search) void runSearch(search.query);
  }

  const conflictCount = conflicts.data?.length ?? 0;
  const tabs: { key: MemoryTab; label: string; signal?: number }[] = [
    { key: "ledger", label: "Ledger" },
    { key: "create", label: "New memory" },
    { key: "correct", label: "Corrections" },
    { key: "conflicts", label: "Conflict inbox", signal: conflictCount || undefined },
  ];

  return (
    <section className={`page ${styles.memoryPage}`} aria-labelledby="memory-heading">
      <PageHeader
        headingId="memory-heading"
        eyebrow="Learn"
        title="Memory ledger"
        description="See what Rusty remembers, whose memory it is, who wrote it, and the evidence behind every record. Corrections write new attributed records — nothing is edited in place."
        actions={<button className="secondary-button" type="button" onClick={refreshEvidence} disabled={searching || conflicts.isFetching}>{searching || conflicts.isFetching ? "Refreshing…" : "Refresh"}</button>}
      />

      <nav className={styles.tabs} aria-label="Memory sections">
        {tabs.map((item) => (
          <button key={item.key} type="button" aria-pressed={tab === item.key} onClick={() => setTab(item.key)}>
            {item.label}
            {item.signal ? <span className={styles.tabSignal}>{item.signal}</span> : null}
          </button>
        ))}
      </nav>

      {tab === "ledger" && (
        <LedgerView
          search={search}
          searching={searching}
          searchError={searchError}
          onSearch={runSearch}
          conflictedIds={conflictedIds}
          selectedId={selectedId}
          onSelect={setSelectedId}
          detail={selectedId ? (
            <MemoryDetail
              memoryId={selectedId}
              records={search?.records ?? []}
              conflicts={conflicts.data ?? []}
              onSelect={inspectRecord}
              onCorrect={correctRecord}
              onForgotten={() => { setSelectedId(""); afterMutation(); }}
              onClose={() => setSelectedId("")}
            />
          ) : null}
        />
      )}
      {tab === "create" && (
        <CreateMemoryPanel onCreated={(receipt) => { afterMutation(); inspectRecord(receipt.memory_id); }} />
      )}
      {tab === "correct" && (
        <CorrectionPanel
          key={correctionTargetId || "blank"}
          initialTargetId={correctionTargetId}
          onSubmitted={() => afterMutation()}
          onInspect={inspectRecord}
        />
      )}
      {tab === "conflicts" && (
        <ConflictInbox conflicts={conflicts} onInspect={inspectRecord} />
      )}
    </section>
  );
}
