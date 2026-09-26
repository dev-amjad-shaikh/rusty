import { VIEWS } from "./Knot";

/** A library view not yet ported: the prototype's page frame, empty. */
export function LibraryView({ view }: { view: string }) {
  const v = VIEWS.find((x) => x.view === view);
  return (
    <div className="view library active" id={`view-${view}`}>
      <div className="lib-top"><div className="crumbs"><b>{v?.title ?? view}</b></div></div>
      <div className="lib-page"><div className="thread-empty" style={{ padding: 40 }}><i className={`ti ${v?.icon ?? "ti-box"}`} />{v?.title ?? view} is being ported next.</div></div>
    </div>
  );
}
