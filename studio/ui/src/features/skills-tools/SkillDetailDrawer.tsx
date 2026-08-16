import { useEffect, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import type { ConnectionIdentity } from "../../lib/api/client";
import {
  getSkill,
  getSkillBody,
  getSkillFile,
  getSkillHistory,
  getSkillVersion,
  type SkillDetail,
  type SkillMetadata,
  type SkillReceipt,
} from "../../lib/api/skills";
import { evidencePreview } from "../../lib/text";
import { publishedMembers } from "./publishedMembers";
import styles from "./SkillsPage.module.css";

function hexPreview(bytes: Uint8Array, max = 128) {
  const shown = Array.from(bytes.slice(0, max), (byte) => byte.toString(16).padStart(2, "0")).join(" ");
  return bytes.byteLength > max ? `${shown} …` : shown;
}

export function SkillDetailDrawer({ connection, scope, name, onClose }: {
  connection: ConnectionIdentity;
  scope: string;
  name: string;
  onClose: () => void;
}) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const closeRef = useRef<HTMLButtonElement>(null);
  const [pinned, setPinned] = useState<number | null>(null);
  const [bodyRequested, setBodyRequested] = useState(false);
  const [memberPath, setMemberPath] = useState("");
  const [memberRequest, setMemberRequest] = useState("");

  useEffect(() => {
    const dialog = dialogRef.current;
    if (dialog && !dialog.open) {
      if (typeof dialog.showModal === "function") dialog.showModal();
      else dialog.setAttribute("open", "");
    }
    requestAnimationFrame(() => closeRef.current?.focus());
    return () => { if (dialog?.open && typeof dialog.close === "function") dialog.close(); };
  }, []);

  const key = [scope, "skills", name];
  const detail = useQuery({ queryKey: [...key, "receipt"], queryFn: () => getSkill(connection, name), retry: false });
  const history = useQuery({
    queryKey: [...key, "history"],
    queryFn: () => getSkillHistory(connection, name),
    enabled: Boolean(detail.data),
    retry: false,
  });
  const version = useQuery({
    queryKey: [...key, "version", pinned],
    queryFn: () => getSkillVersion(connection, name, pinned!),
    enabled: pinned !== null,
    retry: false,
  });
  const body = useQuery({
    queryKey: [...key, "body"],
    queryFn: () => getSkillBody(connection, name),
    enabled: bodyRequested,
    retry: false,
  });
  const member = useQuery({
    queryKey: [...key, "file", memberRequest],
    queryFn: () => getSkillFile(connection, name, memberRequest),
    enabled: Boolean(memberRequest),
    retry: false,
  });

  const viewingPinned = pinned !== null;
  const receipt: SkillReceipt | SkillDetail | null = viewingPinned ? version.data ?? null : detail.data ?? null;
  const latestRevision = detail.data?.revision ?? 0;
  const knownMembers = publishedMembers(scope, name);

  function requestMember(path: string) {
    const exact = path.trim();
    if (exact) setMemberRequest(exact);
  }

  return (
    <dialog ref={dialogRef} className={styles.drawer} aria-labelledby="skill-detail-heading" onCancel={(event) => { event.preventDefault(); onClose(); }}>
      <div className={styles.drawerBody}>
        <header className={styles.drawerHead}>
          <div>
            <span className="eyebrow">Skill package</span>
            <h2 id="skill-detail-heading">{name}</h2>
            <p>{detail.data ? evidencePreview(detail.data.metadata.description, 240) : detail.isLoading ? "Loading receipt…" : "Receipt unavailable."}</p>
          </div>
          <button ref={closeRef} className={styles.closeDrawer} type="button" onClick={onClose}>Close</button>
        </header>
        <div className={styles.drawerScroll}>
          {detail.isLoading ? (
            <div className={styles.loading} role="status">Loading the skill receipt…</div>
          ) : detail.isError ? (
            <div className={styles.error} role="alert">
              <p>{detail.error instanceof Error ? detail.error.message : "The skill receipt could not be loaded."}</p>
              <p><button type="button" className="secondary-button" onClick={() => detail.refetch()}>Retry</button></p>
            </div>
          ) : receipt ? (
            <>
              {viewingPinned && (
                <p className={styles.pinnedNote}>
                  Viewing pinned revision r{pinned} of r{latestRevision}. Instructions and members disclose from the latest revision only.{" "}
                  <button type="button" onClick={() => setPinned(null)}>Back to latest</button>
                </p>
              )}
              <section className={styles.drawerSection} aria-label="Registration receipt">
                <h3>Registration receipt</h3>
                <ReceiptGrid receipt={receipt} totalRevisions={"revisions" in receipt ? (receipt as SkillDetail).revisions : latestRevision} />
              </section>

              {!viewingPinned && (
                <section className={styles.drawerSection} aria-label="Instructions">
                  <h3>Instructions</h3>
                  {!bodyRequested ? (
                    <button className="secondary-button" type="button" onClick={() => setBodyRequested(true)}>Load instructions</button>
                  ) : body.isLoading ? (
                    <div className={styles.loading} role="status">Loading instructions…</div>
                  ) : body.isError ? (
                    <p className={styles.error} role="alert">{body.error instanceof Error ? body.error.message : "Instructions could not be loaded."}</p>
                  ) : body.data ? (
                    <pre className={styles.instructionsPre} aria-label="Skill instructions">{body.data.body}</pre>
                  ) : null}
                </section>
              )}

              {!viewingPinned && (
                <section className={styles.drawerSection} aria-label="Package members">
                  <h3>References & assets</h3>
                  {knownMembers.length > 0 && (
                    <div className={styles.memberChips}>
                      {knownMembers.map((path) => (
                        <button key={path} type="button" aria-pressed={memberRequest === path} onClick={() => requestMember(path)}>{path}</button>
                      ))}
                    </div>
                  )}
                  <form className={styles.memberLookup} onSubmit={(event) => { event.preventDefault(); requestMember(memberPath); }}>
                    <input
                      aria-label="Member path"
                      value={memberPath}
                      onChange={(event) => setMemberPath(event.target.value)}
                      placeholder="references/guide.md"
                    />
                    <button className="secondary-button" type="submit" disabled={!memberPath.trim()}>Fetch member</button>
                  </form>
                  {memberRequest && (
                    member.isLoading ? <p className={styles.memberMeta} role="status">Fetching {memberRequest}…</p>
                    : member.isError ? <p className={styles.error} role="alert">{member.error instanceof Error ? member.error.message : "Member could not be fetched."}</p>
                    : member.data ? (
                      <>
                        <p className={styles.memberMeta}>{member.data.path} · {member.data.bytes.byteLength.toLocaleString()} bytes · {member.data.contentType || "unknown type"}</p>
                        {member.data.contentType.startsWith("text/") ? (
                          <pre className={styles.instructionsPre} aria-label="Member content">{new TextDecoder().decode(member.data.bytes)}</pre>
                        ) : (
                          <pre className={styles.instructionsPre} aria-label="Member bytes">{hexPreview(member.data.bytes)}</pre>
                        )}
                      </>
                    ) : null
                  )}
                </section>
              )}

              <section className={styles.drawerSection} aria-label="Revision history">
                <h3>Revision history</h3>
                {history.isLoading ? (
                  <p className={styles.memberMeta} role="status">Loading history…</p>
                ) : history.isError ? (
                  <p className={styles.error} role="alert">{history.error instanceof Error ? history.error.message : "History could not be loaded."}</p>
                ) : history.data ? (
                  <ol className={styles.historyList}>
                    {history.data.map((entry) => (
                      <li key={entry.revision}>
                        <button type="button" aria-current={(viewingPinned ? pinned : latestRevision) === entry.revision} onClick={() => setPinned(entry.revision === latestRevision ? null : entry.revision)}>
                          <b>r{entry.revision}</b>
                          <code title={entry.content_hash}>{entry.content_hash.slice(0, 16)}…</code>
                          <small>{entry.revision === latestRevision ? "latest" : "pinned"}</small>
                        </button>
                      </li>
                    ))}
                  </ol>
                ) : null}
                {viewingPinned && version.isLoading && <p className={styles.memberMeta} role="status">Loading pinned receipt…</p>}
                {viewingPinned && version.isError && <p className={styles.error} role="alert">{version.error instanceof Error ? version.error.message : "Pinned version could not be loaded."}</p>}
              </section>
            </>
          ) : null}
        </div>
      </div>
    </dialog>
  );
}

function ReceiptGrid({ receipt, totalRevisions }: { receipt: SkillReceipt | SkillMetadata | SkillDetail; totalRevisions: number }) {
  const full = receipt as SkillReceipt;
  const metadata = "metadata" in full ? full.metadata : (receipt as SkillMetadata);
  return (
    <dl className={styles.receiptGrid}>
      <div><dt>Revision</dt><dd>r{receipt.revision} of {totalRevisions}</dd></div>
      <div><dt>Content hash</dt><dd>{receipt.content_hash}</dd></div>
      {"provenance" in full && <div><dt>Published by</dt><dd>{full.provenance.author}</dd></div>}
      {"provenance" in full && <div><dt>Source</dt><dd>{full.provenance.source.type === "registry" ? `registry:${full.provenance.source.name}` : `local:${full.provenance.source.path}`}</dd></div>}
      {"scan" in full && <div><dt>Scan</dt><dd>{full.scan.clean ? "clean — no findings" : `${full.scan.warning_count} recorded warning${full.scan.warning_count === 1 ? "" : "s"}`}</dd></div>}
      {metadata.license && <div><dt>License</dt><dd>{metadata.license}</dd></div>}
      {metadata.allowed_tools?.length ? <div><dt>Allowed tools</dt><dd>{metadata.allowed_tools.join(", ")}</dd></div> : null}
      {metadata.compatibility && <div><dt>Compatibility</dt><dd>{metadata.compatibility}</dd></div>}
    </dl>
  );
}
