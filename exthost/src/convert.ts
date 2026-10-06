// Conversions between LSP wire types (0-based positions) and VS Code's internal
// editor DTOs (1-based positions) that flow across the ExtHost RPC boundary.
// The DTO parameter types are VS Code's own types: the ExtHost side guarantees
// these shapes, so they are the validated boundary types here.
import { MarkerSeverity, CompletionItemKind, CompletionItemInsertTextRule, ISuggestDataDtoField, URI } from './vs.js';
import type { UriComponents } from './vs.js';
import type { IMarkdownString } from '../.vscode-src/src/vs/base/common/htmlContent.js';
import type { IMarkerData } from '../.vscode-src/src/vs/platform/markers/common/markers.js';
import type { IRange } from '../.vscode-src/src/vs/editor/common/core/range.js';
import type * as languages from '../.vscode-src/src/vs/editor/common/languages.js';
import type {
	HoverWithId, ILocationDto, ILocationLinkDto, ISuggestDataDto, IWorkspaceEditDto,
} from '../.vscode-src/src/vs/workbench/api/common/extHost.protocol.js';

// ---- positions & ranges -----------------------------------------------------

export interface LspPosition { line: number; character: number; }
export interface LspRange { start: LspPosition; end: LspPosition; }

export function toInternalPosition(p: LspPosition): { lineNumber: number; column: number } {
	return { lineNumber: p.line + 1, column: p.character + 1 };
}

export function toLspRange(r: IRange): LspRange {
	return {
		start: { line: r.startLineNumber - 1, character: r.startColumn - 1 },
		end: { line: r.endLineNumber - 1, character: r.endColumn - 1 },
	};
}

export function toInternalRange(r: LspRange): IRange {
	return {
		startLineNumber: r.start.line + 1,
		startColumn: r.start.character + 1,
		endLineNumber: r.end.line + 1,
		endColumn: r.end.character + 1,
	};
}

// ---- small boundary readers -------------------------------------------------

function markdownToString(md: string | IMarkdownString | undefined): string {
	if (typeof md === 'string') { return md; }
	if (md && typeof md.value === 'string') { return md.value; }
	return '';
}

function uriToString(uri: UriComponents): string {
	return URI.revive(uri).toString();
}

// ---- hover ------------------------------------------------------------------

export function hoverToLsp(dto: HoverWithId | undefined): unknown {
	if (!dto) { return null; }
	const parts = (dto.contents ?? []).map(markdownToString).filter((s) => s.length > 0);
	const result: Record<string, unknown> = {
		contents: { kind: 'markdown', value: parts.join('\n\n---\n\n') },
	};
	if (dto.range) { result.range = toLspRange(dto.range); }
	return result;
}

// ---- locations --------------------------------------------------------------

export function locationLinksToLsp(links: ILocationLinkDto[] | undefined): unknown[] {
	if (!links) { return []; }
	return links.map((l) => ({ uri: uriToString(l.uri), range: toLspRange(l.targetSelectionRange ?? l.range) }));
}

export function locationsToLsp(locs: ILocationDto[] | undefined): unknown[] {
	if (!locs) { return []; }
	return locs.map((l) => ({ uri: uriToString(l.uri), range: toLspRange(l.range) }));
}

// ---- text edits -------------------------------------------------------------

export function textEditsToLsp(edits: languages.TextEdit[] | undefined): unknown[] {
	if (!edits) { return []; }
	return edits.map((e) => ({ range: toLspRange(e.range), newText: e.text }));
}

// ---- diagnostics ------------------------------------------------------------

const MARKER_SEVERITY_TO_LSP: Record<number, number> = {
	[MarkerSeverity.Error]: 1,
	[MarkerSeverity.Warning]: 2,
	[MarkerSeverity.Info]: 3,
	[MarkerSeverity.Hint]: 4,
};

export function markerToLspDiagnostic(m: IMarkerData): unknown {
	const diag: Record<string, unknown> = {
		range: toLspRange({ startLineNumber: m.startLineNumber, startColumn: m.startColumn, endLineNumber: m.endLineNumber, endColumn: m.endColumn }),
		message: m.message,
		severity: MARKER_SEVERITY_TO_LSP[m.severity] ?? 1,
	};
	if (m.source) { diag.source = m.source; }
	if (m.code !== undefined && m.code !== null) {
		diag.code = typeof m.code === 'object' ? m.code.value : m.code;
	}
	if (m.tags) { diag.tags = m.tags; }
	if (m.relatedInformation) {
		diag.relatedInformation = m.relatedInformation.map((r) => ({
			location: { uri: uriToString(r.resource), range: toLspRange(r) },
			message: r.message,
		}));
	}
	return diag;
}

// ---- completion -------------------------------------------------------------

// Monaco CompletionItemKind -> LSP CompletionItemKind.
const COMPLETION_KIND_TO_LSP: Record<number, number> = {
	[CompletionItemKind.Method]: 2,
	[CompletionItemKind.Function]: 3,
	[CompletionItemKind.Constructor]: 4,
	[CompletionItemKind.Field]: 5,
	[CompletionItemKind.Variable]: 6,
	[CompletionItemKind.Class]: 7,
	[CompletionItemKind.Interface]: 8,
	[CompletionItemKind.Module]: 9,
	[CompletionItemKind.Property]: 10,
	[CompletionItemKind.Unit]: 11,
	[CompletionItemKind.Value]: 12,
	[CompletionItemKind.Enum]: 13,
	[CompletionItemKind.Keyword]: 14,
	[CompletionItemKind.Snippet]: 15,
	[CompletionItemKind.Color]: 16,
	[CompletionItemKind.File]: 17,
	[CompletionItemKind.Reference]: 18,
	[CompletionItemKind.Folder]: 19,
	[CompletionItemKind.EnumMember]: 20,
	[CompletionItemKind.Constant]: 21,
	[CompletionItemKind.Struct]: 22,
	[CompletionItemKind.Event]: 23,
	[CompletionItemKind.Operator]: 24,
	[CompletionItemKind.TypeParameter]: 25,
	[CompletionItemKind.Text]: 1,
};

const F = ISuggestDataDtoField;

export interface DefaultRanges { insert: IRange; replace: IRange; }

export function suggestItemToLsp(item: ISuggestDataDto, defaultRange: DefaultRanges | undefined): unknown {
	const labelRaw = item[F.label];
	const label = typeof labelRaw === 'string' ? labelRaw : labelRaw.label;
	const result: Record<string, unknown> = { label };
	const kind = item[F.kind];
	if (typeof kind === 'number') { result.kind = COMPLETION_KIND_TO_LSP[kind] ?? 1; }
	if (item[F.detail] !== undefined) { result.detail = item[F.detail]; }
	const doc = item[F.documentation];
	if (doc !== undefined) { result.documentation = { kind: 'markdown', value: markdownToString(doc) }; }
	if (item[F.sortText] !== undefined) { result.sortText = item[F.sortText]; }
	if (item[F.filterText] !== undefined) { result.filterText = item[F.filterText]; }
	if (item[F.preselect] !== undefined) { result.preselect = true; }
	const insertText = item[F.insertText] ?? label;
	const rules = item[F.insertTextRules];
	const isSnippet = typeof rules === 'number' && (rules & CompletionItemInsertTextRule.InsertAsSnippet) !== 0;
	result.insertText = insertText;
	result.insertTextFormat = isSnippet ? 2 : 1;
	const range = item[F.range] ?? defaultRange;
	if (range) {
		const r = 'insert' in range ? range.insert : range;
		result.textEdit = { range: toLspRange(r), newText: insertText };
	}
	const commit = item[F.commitCharacters];
	if (typeof commit === 'string') { result.commitCharacters = commit.split(''); }
	const extra = item[F.additionalTextEdits];
	if (extra) { result.additionalTextEdits = extra.map((e) => ({ range: toLspRange(e.range), newText: e.text })); }
	return result;
}

// ---- workspace edits --------------------------------------------------------

export function workspaceEditToLsp(dto: IWorkspaceEditDto | undefined): unknown {
	const changes: Record<string, unknown[]> = {};
	for (const edit of dto?.edits ?? []) {
		if ('resource' in edit && 'textEdit' in edit) {
			const uri = uriToString(edit.resource);
			(changes[uri] ??= []).push({ range: toLspRange(edit.textEdit.range), newText: edit.textEdit.text });
		}
	}
	return { changes };
}

// ---- document symbols -------------------------------------------------------

export function documentSymbolToLsp(sym: languages.DocumentSymbol): unknown {
	return {
		name: sym.name,
		detail: sym.detail ?? '',
		kind: sym.kind + 1, // monaco SymbolKind is 0-based; LSP is 1-based
		range: toLspRange(sym.range),
		selectionRange: toLspRange(sym.selectionRange),
		tags: sym.tags,
		children: (sym.children ?? []).map(documentSymbolToLsp),
	};
}
