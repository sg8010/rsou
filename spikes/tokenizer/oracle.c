/*
 * Stage 0 only: call the legacy FTS5 tokenizer API exported by wsou's
 * libsimple.so and print token/range pairs.  The system SQLite shipped on the
 * validation host exposes FTS5 API version 2, so the v1 xFindTokenizer path is
 * intentional here.  This file is not linked into rsou.
 */
#include <sqlite3.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int print_token(void *ctx, int flags, const char *token, int n_token,
                       int start, int end) {
    (void)ctx;
    (void)flags;
    printf("%.*s\t%d\t%d\n", n_token, token, start, end);
    return SQLITE_OK;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: %s LIBSIMPLE.so TEXT\n", argv[0]);
        return 2;
    }

    sqlite3 *db = NULL;
    char *error = NULL;
    if (sqlite3_open_v2(":memory:", &db, SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE,
                        NULL) != SQLITE_OK) {
        return 3;
    }
    sqlite3_enable_load_extension(db, 1);
    if (sqlite3_load_extension(db, argv[1], "sqlite3_simple_init", &error) != SQLITE_OK) {
        fprintf(stderr, "load libsimple: %s\n", error ? error : "unknown error");
        sqlite3_free(error);
        sqlite3_close(db);
        return 4;
    }

    sqlite3_stmt *statement = NULL;
    fts5_api *api = NULL;
    if (sqlite3_prepare_v2(db, "SELECT fts5(?1)", -1, &statement, NULL) != SQLITE_OK ||
        sqlite3_bind_pointer(statement, 1, &api, "fts5_api_ptr", NULL) != SQLITE_OK) {
        sqlite3_finalize(statement);
        sqlite3_close(db);
        return 5;
    }
    sqlite3_step(statement);
    sqlite3_finalize(statement);
    if (api == NULL || api->xFindTokenizer == NULL) {
        fprintf(stderr, "SQLite FTS5 API v1 is unavailable\n");
        sqlite3_close(db);
        return 6;
    }

    void *user_data = NULL;
    fts5_tokenizer tokenizer_api;
    memset(&tokenizer_api, 0, sizeof(tokenizer_api));
    int rc = api->xFindTokenizer(api, "simple", &user_data, &tokenizer_api);
    if (rc != SQLITE_OK || tokenizer_api.xCreate == NULL) {
        fprintf(stderr, "simple tokenizer not found (rc=%d)\n", rc);
        sqlite3_close(db);
        return 7;
    }

    Fts5Tokenizer *tokenizer = NULL;
    const char *args[] = {"0"};
    rc = tokenizer_api.xCreate(user_data, args, 1, &tokenizer);
    if (rc != SQLITE_OK || tokenizer == NULL) {
        fprintf(stderr, "simple tokenizer creation failed (rc=%d)\n", rc);
        sqlite3_close(db);
        return 8;
    }
    rc = tokenizer_api.xTokenize(tokenizer, NULL, FTS5_TOKENIZE_DOCUMENT, argv[2],
                                 (int)strlen(argv[2]), print_token);
    tokenizer_api.xDelete(tokenizer);
    sqlite3_close(db);
    return rc == SQLITE_OK ? 0 : 9;
}
