#!/usr/bin/perl
use strict;
use warnings;

# Greet everyone
sub greet {
    my ($name, $count) = @_;
    my %seen = (a => 1, b => 2);
    print "Hello $name\n" for 1 .. $count;
    return scalar(keys %seen) > 0 ? 1 : undef;
}

my @names = qw(alice bob);
greet($_, 2) foreach @names;
if ($names[0] =~ /^a/i) { print 'match', "\n"; }
