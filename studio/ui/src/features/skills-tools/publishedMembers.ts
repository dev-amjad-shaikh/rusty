// The /skills contract discloses member bytes by exact path but does not
// enumerate them, so Studio remembers the member paths of packages it
// published this session and offers them as one-click reads. Anything else
// is reachable through the drawer's exact-path lookup.
const published = new Map<string, string[]>();

export function rememberPublishedMembers(scope: string, name: string, paths: string[]) {
  published.set(`${scope}${name}`, paths.slice(0, 256));
}

export function publishedMembers(scope: string, name: string): string[] {
  return published.get(`${scope}${name}`) ?? [];
}

export function clearPublishedMembers() {
  published.clear();
}
