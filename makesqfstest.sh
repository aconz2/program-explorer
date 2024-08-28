extra="-fsanitize=address"
gcc $extra -g -Wall -o sqfstest -lsquashfs sqfstest.c

# ls sqfstest.c makesqfstest.sh | entr -c bash -c 'bash makesqfstest.sh && ./sqfstest'
