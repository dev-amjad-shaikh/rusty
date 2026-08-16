import { useEffect, useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { connectionScope, StudioApiError } from "../../lib/api/client";
import { listMemoryConflicts, queryMemory, type MemoryQueryInput, type MemoryRecord } from "../../lib/api/memory";
import { useConnectionStore } from "../../state/connection";
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
  const { connection, openDialog } = useConnectionStore();
  const queryClient = useQueryClient();
  const scope = connection ? connectionScope(connection) : "disconnected";
  const [tab, setTab] = useState<MemoryTab>("ledger");
  const [search, setSearch] = useState<SearchState | null>(null);
  const [searchError, setSearchError] = useState("");
  const [searching, setSearching] = useState(false);
  const [selectedId, setSelectedId] = useState("");
  const [correctionTargetId, setCorrectionTargetId] = useState("");

  const conflicts = useQuery({
    queryKey: connection ? [connection.epoch, connection.origin, connection.tenantFingerprint, "memory-conflicts"] : ["memory-conflicts", "disconnected"],
    queryFn: () => listMemoryConflicts(connection!),
    enabled: Boolean(connection),
  });

  useEffect(() => {
    setTab("ledger");
    setSearch(null);
    setSearchError("");
    setSelectedId("");
    setCorrectionTargetId("");
  }, [connection?.epoch, connection?.origin, connection?.tenantFingerprint]);

  const conflictedIds = useMemo(
    () => new Set((conflicts.data ?? []).flatMap((conflict) => conflict.memory_ids)),
    [conflicts.data],
  );

  async function runSearch(query: MemoryQueryInput) {
    if (!connection) return;
    const scopeAtStart = scope;
    setSearching(true);
    setSearchError("");
    try {
      const records = await queryMemory(connection, query);
      const current = useConnectionStore.getState().connection;
      if (!current || connectionScope(current) !== scopeAtStart) return;
      setSearch({ query, records, searchedAt: new Date() });
      setSelectedId("");
    } catch (caught) {
      const current = useConnectionStore.getState().connection;
      if (!current || connectionScope(current) !== scopeAtStart) return;
      setSearch(null);
      setSearchError(caught instanceof StudioApiError ? caught.message : "The memory query could not be completed.");
    } finally {
      const current = useConnectionStore.getState().connection;
      if (current && connectionScope(current) === scopeAtStart) setSearching(false);
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
    void queryClient.invalidateQueries({ queryKey: connection ? [connection.epoch, connection.origin, connection.tenantFingerprint, "memory-conflicts"] : undefined });
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
        actions={connection
          ? <button className="secondary-button" type="button" onClick={refreshEvidence} disabled={searching || conflicts.isFetching}>{searching || conflicts.isFetching ? "Refreshing…" : "Refresh"}</button>
          : <button className="primary-button" type="button" onClick={openDialog}>Choose workspace</button>}
      />

      {!connection ? (
        <div className="empty-state">
          <span className="eyebrow">Governed memory</span>
          <h2>Open a workspace to inspect memory</h2>
          <p>The ledger reads the tenant's governed memory namespace: content-addressed, immutable records with mandatory provenance.</p>
          <button className="primary-button" type="button" onClick={openDialog}>Choose workspace</button>
        </div>
      ) : (
        <>
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
        </>
      )}
    </section>
  );
}
