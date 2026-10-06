// Minimal glob matcher sufficient for workspace.findFiles include/exclude
// patterns. Supports **, *, ?, and {a,b} alternation against POSIX-style paths.
export function globToRegExp(glob: string): RegExp {
	let re = '';
	for (let i = 0; i < glob.length; i++) {
		const c = glob[i];
		if (c === '*') {
			if (glob[i + 1] === '*') {
				i++;
				if (glob[i + 1] === '/') { i++; }
				re += '(?:.*/)?';
			} else {
				re += '[^/]*';
			}
		} else if (c === '?') {
			re += '[^/]';
		} else if (c === '{') {
			re += '(?:';
		} else if (c === '}') {
			re += ')';
		} else if (c === ',') {
			re += '|';
		} else if ('\\^$+.()|[]'.includes(c)) {
			re += `\\${c}`;
		} else {
			re += c;
		}
	}
	return new RegExp(`^${re}$`);
}
