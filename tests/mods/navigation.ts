export async function goto(url: string | URL) {
	(window as unknown as { navigations: string[] }).navigations.push(String(url));
}
