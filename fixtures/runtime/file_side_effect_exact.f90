! Differential filesystem-side-effect witness.
!
! Explicit formatting makes both the console record and the persisted file
! bytes part of the source contract. List-directed output is intentionally not
! used here because its spacing is processor dependent.
!
! CHECK: armfortas-file-side-effect-v1
! FILE_CHECK: differential_artifact.txt => armfortas-file-side-effect-v1
program file_side_effect_exact
    implicit none

    character(len=64) :: payload

    open(10, file='differential_artifact.txt', status='replace')
    write(10, '(A)') 'armfortas-file-side-effect-v1'
    close(10)

    payload = ''
    open(10, file='differential_artifact.txt', status='old')
    read(10, '(A)') payload
    close(10)

    write(*, '(A)') trim(payload)
end program file_side_effect_exact
