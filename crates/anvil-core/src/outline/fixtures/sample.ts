import { readFile } from "node:fs/promises";
import type { Stats } from "node:fs";

/** A user of the service. */
export interface User {
  id: string;
  name: string;
}

/** Where users are kept. */
export class UserStore {
  private users = new Map<string, User>();

  /** Adds or replaces a user. */
  put(user: User): void {
    this.users.set(user.id, user);
  }

  /** The user with this id, if any. */
  get(id: string): User | undefined {
    return this.users.get(id);
  }
}

/** Loads users from a JSON file. */
export async function loadUsers(path: string): Promise<UserStore> {
  const store = new UserStore();
  for (const user of JSON.parse(await readFile(path, "utf8")) as User[]) {
    store.put(user);
  }
  return store;
}

/** Formats a user for display. */
export const describe = (user: User): string => `${user.name} (${user.id})`;

export type UserId = User["id"];

const LIMIT = 10;
