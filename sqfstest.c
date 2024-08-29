#include <sys/stat.h>

#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>

#include "sqfs/io.h"
#include "sqfs/super.h"
#include "sqfs/block.h"
#include "sqfs/inode.h"
#include "sqfs/meta_writer.h"
#include "sqfs/compressor.h"
#include "sqfs/frag_table.h"
#include "sqfs/block_writer.h"
#include "sqfs/block_processor.h"
#include "sqfs/xattr_writer.h"
#include "sqfs/id_table.h"
#include "sqfs/dir_writer.h"

void errexit(const char* message) {
    fputs(message, stderr);
    exit(1);
}

// based a bit on https://docs.rs/squashfs-ng/latest/src/squashfs_ng/write.rs.html
// TODO errs https://github.com/AgentD/squashfs-tools-ng/blob/master/lib/common/src/perror.c#L11
// this might be more helpful https://github.com/AgentD/squashfs-tools-ng/blob/e3dcf1770fd77a0babcca422dcbe7b2cc7b8ab90/extras/mknastyfs.c#L130

int main() {
    const char* outfile = "/tmp/sqfstest.sqfs";
    sqfs_file_t* sf = sqfs_open_file(outfile, SQFS_FILE_OPEN_OVERWRITE);
    if (sf == NULL) {
        errexit("sqfs_open_file fail");
    }

    sqfs_compressor_config_t cc;
    if (sqfs_compressor_config_init(&cc, SQFS_COMP_ZSTD, SQFS_DEFAULT_BLOCK_SIZE, 0) != 0) {
        errexit("sqfs_compressor_config_init fail");
    }

    sqfs_compressor_t* comp;
    if (sqfs_compressor_create(&cc, &comp) != 0) {
        errexit("sqfs_compressor_create fail");
    }

    sqfs_frag_table_t* ft = sqfs_frag_table_create(0);
    if (ft == NULL) {
        errexit("sqfs_frag_table_create fail");
    }

    sqfs_block_writer_t* bw = sqfs_block_writer_create(sf, 4096, 0);
    if (bw == NULL) {
        errexit("sqfs_block_writer_create fail");
    }

    sqfs_xattr_writer_t* xw = sqfs_xattr_writer_create(0);
    if (xw == NULL) {
        errexit("sqfs_xattr_writer_create fail");
    }

    sqfs_id_table_t* idt = sqfs_id_table_create(0);
    if (idt == NULL) {
        errexit("sqfs_id_table_create fail");
    }

    sqfs_meta_writer_t* dir_meta_writer = sqfs_meta_writer_create(sf, comp, SQFS_META_WRITER_KEEP_IN_MEMORY);
    if (dir_meta_writer == NULL) {
        errexit("sqfs_meta_writer_create");
    }

    sqfs_meta_writer_t* iw = sqfs_meta_writer_create(sf, comp, 0);
    if (iw == NULL) {
        errexit("sqfs_meta_writer_create inode_meta_writer");
    }

    sqfs_dir_writer_t* dw = sqfs_dir_writer_create(dir_meta_writer, 0);
    if (dw == NULL) {
        errexit("sqfs_dir_writer_create");
    }

    sqfs_block_processor_t* bp = sqfs_block_processor_create(
            SQFS_DEFAULT_BLOCK_SIZE, // max_block_size
            comp,
            1, // num workers
            1, // max backlog blocks
            bw,
            ft
            );

    sqfs_super_t super;
    sqfs_super_init(&super, SQFS_DEFAULT_BLOCK_SIZE, 0, SQFS_COMP_ZSTD);

    // ---------- ADD FILE
    sqfs_inode_generic_t* inode;
    if (sqfs_block_processor_begin_file(bp, &inode, NULL, SQFS_BLK_DONT_COMPRESS) != 0) {
        errexit("sqfs_block_processor_begin_file fail");
    }
    if (inode == NULL) {
        errexit("inode * is nullptr");
    }
    printf("got inode_number %d\n", inode->base.inode_number);

    const char s1[] = "aaaaaaa data for a.txt";
    if (sqfs_block_processor_append(bp, s1, sizeof(s1)) != 0) {
        errexit("sqfs_block_processor_append fail");
    }

    if (sqfs_block_processor_end_file(bp) != 0) {
        errexit("sqfs_block_processor_end_file fail");
    }

    inode->base.type = SQFS_INODE_FILE;
    inode->base.mode = SQFS_INODE_MODE_REG | 0644;
    inode->base.inode_number = 0;
    inode->data.file.file_size = sizeof(s1);
    //-----------------

    // ---------- WRITE DIR
    if (sqfs_dir_writer_begin(dw, 0) != 0) {
        errexit("sqfs_dir_writer_begin");
    }

    uint64_t block = 0;
    uint32_t offset = 0;
    sqfs_meta_writer_get_position(iw, &block, &offset);

    if (sqfs_dir_writer_add_entry(dw, "a.txt", inode->base.inode_number, block << 16 | offset, S_IFREG | 0777) != 0) {
        errexit("sqfs_dir_writer_add_entry fail");
    }
    //

    // https://github.com/AgentD/squashfs-tools-ng/blob/e3dcf1770fd77a0babcca422dcbe7b2cc7b8ab90/lib/common/src/writer/finish.c#L124
    // block_processor_finish
    // frag_table_write
    // id_table_write
    // super_write
    if (sqfs_block_processor_finish(bp) != 0) {
        errexit("sqfs_block_processor_finish");
    }
    
    if (sqfs_frag_table_write(ft, sf, &super, comp) != 0) {
        errexit("sqfs_frag_table_write fail");
    }
    
    if (sqfs_id_table_write(idt, sf, &super, comp) != 0) {
        errexit("sqfs_id_table_write fail");
    }

    // looks like we can skip writing the xattr table
    if (sqfs_super_write(&super, sf) != 0) {
        errexit("sqfs_super_write");
    }


    sqfs_destroy(sf);
    sqfs_destroy(comp);
    sqfs_destroy(ft);
    sqfs_destroy(bw);
    sqfs_destroy(bp);
    sqfs_destroy(xw);
    sqfs_destroy(idt);

    puts("done");
    return 0;
}
