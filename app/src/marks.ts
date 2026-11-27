import type { CommandInfo, PromptPart, SkillInfo } from "@nabu/protocol";
import { demoMode } from "./chat/actions";
import { demoCommands, demoSkills } from "./data";
import { request } from "./server";

export type Mark = { kind: "skill" | "command"; name: string; sign: "/" | "@" };
/** What the composer holds: typed text and the marks between it. */
export type Piece = string | Mark;
export type Choice = { kind: Mark["kind"]; name: string; description: string };

/** Marks one message may hold; the server takes as many skills. */
export const maxMarks = 10;

export const markText = (mark: Mark) => `${mark.sign}${mark.name}`;
export const plainText = (pieces: Piece[]) => pieces.map(piece => typeof piece === "string" ? piece : markText(piece)).join("");
export const marksOf = (pieces: Piece[]) => pieces.filter((piece): piece is Mark => typeof piece !== "string");
export const hasContent = (pieces: Piece[]) => pieces.some(piece => typeof piece !== "string" || !!piece.trim());

/** The pieces as a message: the text around marks keeps its spaces, the message's own ends are trimmed. */
export function promptParts(pieces: Piece[], newId: () => string): PromptPart[] {
  const parts: PromptPart[] = [];
  pieces.forEach((piece, index) => {
    if (typeof piece !== "string") {
      parts.push(piece.kind === "skill"
        ? { type: "skill", id: newId(), name: piece.name, ...(piece.sign === "@" ? { at: true } : {}) }
        : { type: "command", id: newId(), name: piece.name });
      return;
    }
    let text = piece;
    if (index === 0) text = text.trimStart();
    if (index === pieces.length - 1) text = text.trimEnd();
    if (text) parts.push({ type: "text", id: newId(), text });
  });
  return parts;
}

type MarkedPart = { type: "text"; text: string } | { type: "skill"; name: string; at?: boolean } | { type: "command"; name: string } | { type: "mark"; mark: Mark };

/** A message's text and marks, sent or shown, as composer pieces; texts next to each other were separate paragraphs. */
export function piecesOf(parts: readonly (MarkedPart | { type: string })[]): Piece[] {
  const pieces: Piece[] = [];
  for (const part of parts as MarkedPart[]) {
    if (part.type === "text") {
      const last = pieces.at(-1);
      if (typeof last === "string") pieces[pieces.length - 1] = `${last}\n\n${part.text}`;
      else pieces.push(part.text);
    } else if (part.type === "mark") pieces.push(part.mark);
    else if (part.type === "skill" || part.type === "command") pieces.push(markOf(part));
  }
  return pieces;
}

export const markOf = (part: { type: "skill"; name: string; at?: boolean } | { type: "command"; name: string }): Mark =>
  ({ kind: part.type, name: part.name, sign: part.type === "skill" && part.at ? "@" : "/" });

/** Joins a piece list in front of another, a paragraph apart. */
export function prepend(first: Piece[], rest: Piece[]): Piece[] {
  if (!hasContent(rest)) return first;
  return [...first, "\n\n", ...rest];
}

// Skills by project folder; each opening of the list asks again, the last answer shows meanwhile
const known = new Map<string, Choice[]>();
let commands: Choice[] | undefined;

const skillChoice = (skill: SkillInfo): Choice => ({ kind: "skill", name: skill.name, description: skill.description });
const commandChoice = (command: CommandInfo): Choice => ({ kind: "command", name: command.name, description: command.description });

export async function loadChoices(directory: string): Promise<Choice[]> {
  if (demoMode) return [...demoCommands.map(commandChoice), ...demoSkills.map(skillChoice)];
  const [skills, listed] = await Promise.all([
    request("skills", { directory }).then(list => list.filter(skill => skill.enabled).map(skillChoice)),
    commands ? Promise.resolve(commands) : request("commands").then(list => list.map(commandChoice)),
  ]);
  commands = listed;
  known.set(directory, skills);
  return [...listed, ...skills];
}

export const knownChoices = (directory: string) => [...(commands ?? []), ...(known.get(directory) ?? [])];
