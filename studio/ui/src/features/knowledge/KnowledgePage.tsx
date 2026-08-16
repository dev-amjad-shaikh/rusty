import { useState } from "react";
import type { ConnectionIdentity } from "../../lib/api/client";
import { useConnectionStore } from "../../state/connection";
import { PageHeader } from "../../components/PageHeader";
import { SourceLibrary } from "./SourceLibrary";
import { RegisterSourceForm } from "./RegisterSourceForm";
import { SourceDetail } from "./SourceDetail";
import { QueryConsole } from "./QueryConsole";
import { RetentionPanel } from "./RetentionPanel";
import styles from "./KnowledgePage.module.css";

type KnowledgeView =
  | { tab: "sources" }
  | { tab: "register" }
  | { tab: "source"; sourceId: string }
  | { tab: "query" }
  | { tab: "retention" };

const mainTabs = [
  { tab: "sources" as const, label: "Sources" },
  { tab: "query" as const, label: "Query console" },
  { tab: "retention" as const, label: "Retention" },
];

export function KnowledgePage() {
  const { connection, openDialog } = useConnectionStore();
  const [view, setView] = useState<KnowledgeView>({ tab: "sources" });
  const activeTab = view.tab === "register" || view.tab === "source" ? "sources" : view.tab;

  return (
    <section className={`${styles.knowledge} page`} aria-labelledby="knowledge-heading">
      {view.tab === "register" || view.tab === "source" ? (
        <PageHeader
          variant="compact"
          headingId="knowledge-heading"
          eyebrow="Learn · Knowledge"
          title={view.tab === "register" ? "Register source" : "Source detail"}
          description={view.tab === "register"
            ? "A governed source: attributed, content-addressed, and chunked for cited retrieval."
            : "Metadata, chunks, and the correction chain of one source."}
          actions={<button className={styles.backButton} type="button" onClick={() => setView({ tab: "sources" })}>
            ← Back to source library
          </button>}
        />
      ) : (
        <PageHeader
          headingId="knowledge-heading"
          eyebrow="Learn"
          title="Knowledge"
          description="Governed sources agents retrieve from — every answer cites the exact chunk."
          actions={<div style={{ display: "flex", gap: 8 }}>
            <button className="secondary-button" type="button" onClick={() => setView({ tab: "query" })}>Test retrieval</button>
            <button className="primary-button" type="button" onClick={() => setView({ tab: "register" })}>Register source</button>
          </div>}
        />
      )}

      <nav className={styles.tabs} aria-label="Knowledge sections">
        {mainTabs.map(({ tab, label }) => (
          <button
            key={tab}
            type="button"
            className={`${styles.tab} ${activeTab === tab ? styles.activeTab : ""}`}
            aria-current={activeTab === tab ? "page" : undefined}
            onClick={() => setView({ tab })}
          >{label}</button>
        ))}
      </nav>

      {!connection ? (
        <DisconnectedState openDialog={openDialog} />
      ) : view.tab === "register" ? (
        <RegisterSourceForm
          connection={connection}
          onDone={(sourceId) => setView({ tab: "source", sourceId })}
          onCancel={() => setView({ tab: "sources" })}
        />
      ) : view.tab === "source" ? (
        <SourceDetail
          connection={connection}
          sourceId={view.sourceId}
          onBack={() => setView({ tab: "sources" })}
        />
      ) : view.tab === "query" ? (
        <QueryConsole connection={connection} />
      ) : view.tab === "retention" ? (
        <RetentionPanel connection={connection} />
      ) : (
        <SourceLibrary
          connection={connection}
          onOpenSource={(sourceId) => setView({ tab: "source", sourceId })}
          onRegister={() => setView({ tab: "register" })}
        />
      )}
    </section>
  );
}

function DisconnectedState({ openDialog }: { openDialog: () => void }) {
  return (
    <div className={styles.emptyState}>
      <span className={styles.emptyMark} aria-hidden="true">K</span>
      <div>
        <h2>Knowledge needs a workspace</h2>
        <p>Sources, retrieval, and retention live in the connected tenant. Choose a workspace to open its library.</p>
      </div>
      <div><button className="primary-button" type="button" onClick={openDialog}>Choose workspace</button></div>
    </div>
  );
}

export type { ConnectionIdentity as KnowledgeConnection };
